//! Git hook discovery and execution for embedders and the CLI.
//!
//! Traditional `$GIT_DIR/hooks/<name>` scripts and configured `hook.*` commands
//! from git config are both supported. Callers that inject `-c` / `GIT_CONFIG_*`
//! overrides (as the CLI does) should supply them via [`HookEnvironment`].

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use sley_config::{ConfigParameter, ConfigSection};
use sley_core::{GitError, Result};
use sley_odb::repository_common_dir;
use sley_worktree::{
    discover_git_dir_respecting_environment, environment_work_tree, resolve_path_from_cwd,
    worktree_root_for_git_dir,
};

#[derive(Clone)]
enum HookCommand {
    Traditional(PathBuf),
    Configured {
        name: String,
        command: String,
        disabled: bool,
        event_disabled: bool,
        parallel: bool,
        scope: &'static str,
    },
}

struct ScopedSection {
    scope: &'static str,
    section: ConfigSection,
}

/// Optional process-level inputs that affect hook discovery and execution.
#[derive(Debug, Clone, Default)]
pub struct HookEnvironment {
    /// Command-line/environment config parameters (`-c`, `GIT_CONFIG_COUNT`, …).
    /// When `None`, [`sley_config::injected_config_parameters`] is read from the
    /// process environment.
    pub injected_config: Option<Vec<ConfigParameter>>,
    /// Explicit git directory (`--git-dir`, embedder override). When set, hook
    /// discovery uses this instead of walk-up / `GIT_DIR` env discovery alone.
    pub git_dir: Option<PathBuf>,
}

impl HookEnvironment {
    /// Build hook environment from the current process (including
    /// `GIT_CONFIG_PARAMETERS` when set).
    pub fn from_process() -> Self {
        Self {
            injected_config: sley_config::injected_config_parameters(None).ok(),
            git_dir: None,
        }
    }
}

/// Options controlling one hook invocation.
#[derive(Clone)]
pub struct HookRun {
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
    pub stdout_to_stderr: bool,
    pub error_if_missing: bool,
    pub cwd: Option<PathBuf>,
    pub git_dir: Option<PathBuf>,
    pub normalize_failure: bool,
    pub jobs: Option<usize>,
    pub force_serial: bool,
}

impl Default for HookRun {
    fn default() -> Self {
        Self {
            args: Vec::new(),
            stdin: None,
            env: Vec::new(),
            stdout_to_stderr: true,
            error_if_missing: false,
            cwd: None,
            git_dir: None,
            normalize_failure: true,
            jobs: None,
            force_serial: false,
        }
    }
}

/// Hook event names recognized by `git hook list` / `git hook run`.
pub const KNOWN_HOOKS: &[&str] = &[
    "applypatch-msg",
    "commit-msg",
    "fsmonitor-watchman",
    "post-applypatch",
    "post-checkout",
    "post-commit",
    "post-merge",
    "post-receive",
    "post-rewrite",
    "post-update",
    "pre-applypatch",
    "pre-auto-gc",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "pre-receive",
    "prepare-commit-msg",
    "proc-receive",
    "push-to-checkout",
    "reference-transaction",
    "sendemail-validate",
    "update",
];

/// Run `git hook` (`list` / `run` subcommands).
pub fn cmd_hook(args: &[String]) -> Result<()> {
    cmd_hook_with_env(args, &HookEnvironment::from_process())
}

/// Run `git hook` with caller-provided CLI/environment overrides.
pub fn cmd_hook_with_env(args: &[String], hook_env: &HookEnvironment) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => cmd_hook_list(&args[1..], hook_env),
        Some("run") => cmd_hook_run(&args[1..], hook_env),
        _ => {
            hook_usage();
            Err(GitError::Exit(129))
        }
    }
}

/// Run all hooks registered for `hook_name`.
pub fn run_hook(hook_name: &str, options: HookRun, hook_env: &HookEnvironment) -> Result<bool> {
    let config = hook_config(hook_env);
    let hooks = list_hook_commands_with_config(hook_name, hook_env, &config)?;
    if hooks.is_empty() {
        if options.error_if_missing {
            eprintln!("error: cannot find a hook named {hook_name}");
            return Err(GitError::Exit(1));
        }
        return Ok(false);
    }
    let mut options = options;
    if options.git_dir.is_none() {
        if let Some(git_dir) = hook_env.git_dir.clone() {
            options.git_dir = Some(git_dir);
        } else if let Ok(cwd) = env::current_dir()
            && let Ok(git_dir) = discover_git_dir_respecting_environment(&cwd)
        {
            options.git_dir = Some(git_dir);
        }
    }
    let runnable = hooks
        .into_iter()
        .filter(|hook| {
            !matches!(
                hook,
                HookCommand::Configured { disabled: true, .. }
                    | HookCommand::Configured {
                        event_disabled: true,
                        ..
                    }
            )
        })
        .collect::<Vec<_>>();
    if runnable.is_empty() {
        return Ok(false);
    }
    let jobs = resolve_hook_jobs(hook_name, &options, &config, &runnable);
    if jobs > 1 {
        return run_hooks_parallel(hook_name, &runnable, &mut options, jobs);
    }
    trace_hook_region(hook_name, jobs);
    for hook in runnable {
        let status = spawn_hook(&hook, &options)?;
        if !status.success() {
            return Err(GitError::Exit(hook_failure_code(status.code(), &options)));
        }
    }
    Ok(true)
}

/// Convenience wrapper around [`run_hook`] with string-slice arguments.
pub fn run_hook_l(hook_name: &str, args: &[&str], hook_env: &HookEnvironment) -> Result<bool> {
    run_hook(
        hook_name,
        HookRun {
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            ..HookRun::default()
        },
        hook_env,
    )
}

/// Run the `post-index-change` hook with git's two boolean arguments.
pub fn run_post_index_change_hook(
    updated_workdir: bool,
    updated_skipworktree: bool,
    hook_env: &HookEnvironment,
) -> Result<bool> {
    run_hook_l(
        "post-index-change",
        &[
            if updated_workdir { "1" } else { "0" },
            if updated_skipworktree { "1" } else { "0" },
        ],
        hook_env,
    )
}

/// Return whether any runnable hook is configured for `hook_name`.
pub fn hook_exists(hook_name: &str, hook_env: &HookEnvironment) -> Result<bool> {
    let config = hook_config(hook_env);
    Ok(
        list_hook_commands_with_config(hook_name, hook_env, &config)?
            .into_iter()
            .any(|hook| {
                matches!(hook, HookCommand::Traditional(_))
                    || matches!(
                        hook,
                        HookCommand::Configured {
                            disabled: false,
                            event_disabled: false,
                            ..
                        }
                    )
            }),
    )
}

/// Run the traditional `$GIT_DIR/hooks/reference-transaction` hook for one
/// phase, feeding the queued `<old> <new> <refname>` lines on stdin. Returns
/// `Ok(true)` when the hook ran and exited nonzero (the caller decides whether
/// that aborts the transaction — only the `preparing`/`prepared` phases do),
/// `Ok(false)` when the hook is absent or exited zero, and `Err` only on a spawn
/// I/O failure.
pub fn run_reference_transaction_hook_at(
    git_dir: &Path,
    phase: &str,
    stdin: Vec<u8>,
) -> Result<bool> {
    let common_git_dir = repository_common_dir(git_dir);
    let path = common_git_dir.join("hooks").join("reference-transaction");
    if !is_executable_file(&path) {
        return Ok(false);
    }
    let options = HookRun {
        args: vec![phase.to_string()],
        stdin: Some(stdin),
        env: Vec::new(),
        stdout_to_stderr: false,
        error_if_missing: false,
        cwd: Some(hook_cwd_for_git_dir(git_dir)?),
        git_dir: Some(git_dir.to_path_buf()),
        normalize_failure: false,
        jobs: Some(1),
        force_serial: true,
    };
    let status = spawn_hook(&HookCommand::Traditional(path), &options)?;
    Ok(!status.success())
}

/// Run a traditional hook script at `$GIT_DIR/hooks/<hook_name>`.
pub fn run_traditional_hook_at(git_dir: &Path, hook_name: &str, options: HookRun) -> Result<bool> {
    let common_git_dir = repository_common_dir(git_dir);
    let path = common_git_dir.join("hooks").join(hook_name);
    if !is_executable_file(&path) {
        return Ok(false);
    }
    let mut options = options;
    if options.cwd.is_none() {
        options.cwd = Some(hook_cwd_for_git_dir(git_dir)?);
    }
    if options.git_dir.is_none() {
        options.git_dir = Some(git_dir.to_path_buf());
    }
    let status = spawn_hook(&HookCommand::Traditional(path), &options)?;
    if !status.success() {
        return Err(GitError::Exit(hook_failure_code(status.code(), &options)));
    }
    Ok(true)
}

fn cmd_hook_list(args: &[String], hook_env: &HookEnvironment) -> Result<()> {
    let mut allow_unknown = false;
    let mut nul = false;
    let mut show_scope = false;
    let mut hook_name = None::<String>;
    for arg in args {
        match arg.as_str() {
            "--allow-unknown-hook-name" => allow_unknown = true,
            "-z" => nul = true,
            "--show-scope" => show_scope = true,
            "-h" | "--help" => {
                hook_list_usage();
                return Err(GitError::Exit(129));
            }
            value if value.starts_with('-') => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return Err(GitError::Exit(129));
            }
            value => {
                if hook_name.is_some() {
                    hook_list_usage();
                    return Err(GitError::Exit(129));
                }
                hook_name = Some(value.to_string());
            }
        }
    }
    let Some(hook_name) = hook_name else {
        hook_list_usage();
        return Err(GitError::Exit(129));
    };
    if !allow_unknown && !KNOWN_HOOKS.contains(&hook_name.as_str()) {
        eprintln!("error: unknown hook event '{hook_name}';");
        eprintln!("use --allow-unknown-hook-name to allow non-native hook names");
        return Err(GitError::Exit(1));
    }
    let hooks = list_hook_commands(&hook_name, hook_env)?;
    if hooks.is_empty() {
        eprintln!("warning: no hooks found for event '{hook_name}'");
        return Err(GitError::Exit(1));
    }
    let terminator = if nul { "\0" } else { "\n" };
    let mut stdout = io::stdout().lock();
    for hook in hooks {
        match hook {
            HookCommand::Traditional(_) => write!(stdout, "hook from hookdir{terminator}")?,
            HookCommand::Configured {
                name,
                disabled,
                event_disabled,
                scope,
                ..
            } => {
                let disability = if event_disabled {
                    Some("event-disabled")
                } else if disabled {
                    Some("disabled")
                } else {
                    None
                };
                if show_scope {
                    write!(stdout, "{scope}\t")?;
                    if let Some(disability) = disability {
                        write!(stdout, "{disability}\t")?;
                    }
                    write!(stdout, "{name}{terminator}")?;
                } else if let Some(disability) = disability {
                    write!(stdout, "{disability}\t{name}{terminator}")?;
                } else {
                    write!(stdout, "{name}{terminator}")?;
                }
            }
        }
    }
    Ok(())
}

fn cmd_hook_run(args: &[String], hook_env: &HookEnvironment) -> Result<()> {
    let mut allow_unknown = false;
    let mut ignore_missing = false;
    let mut stdin_path = None::<String>;
    let mut jobs = None::<usize>;
    let mut hook_name = None::<String>;
    let mut hook_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--allow-unknown-hook-name" => allow_unknown = true,
            "--ignore-missing" => ignore_missing = true,
            "--to-stdin" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    hook_run_usage();
                    return Err(GitError::Exit(129));
                };
                stdin_path = Some(path.clone());
            }
            value if value.starts_with("--to-stdin=") => {
                stdin_path = Some(value["--to-stdin=".len()..].to_string());
            }
            "-j" | "--jobs" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    hook_run_usage();
                    return Err(GitError::Exit(129));
                };
                jobs = Some(parse_hook_jobs_arg(value)?);
            }
            value if value.starts_with("--jobs=") => {
                jobs = Some(parse_hook_jobs_arg(&value["--jobs=".len()..])?);
            }
            value if value.starts_with("-j") && value.len() > 2 && hook_name.is_none() => {
                jobs = Some(parse_hook_jobs_arg(&value[2..])?);
            }
            "-h" | "--help" => {
                hook_run_usage();
                return Err(GitError::Exit(129));
            }
            "--" | "--end-of-options" => {
                hook_args.extend(args[index + 1..].iter().cloned());
                break;
            }
            value if value.starts_with('-') && hook_name.is_none() => {
                eprintln!("error: unknown option `{}`", value.trim_start_matches('-'));
                return Err(GitError::Exit(129));
            }
            value => {
                if hook_name.is_some() {
                    hook_run_usage();
                    return Err(GitError::Exit(129));
                }
                hook_name = Some(value.to_string());
            }
        }
        index += 1;
    }
    let Some(hook_name) = hook_name else {
        hook_run_usage();
        return Err(GitError::Exit(129));
    };
    if !allow_unknown && !KNOWN_HOOKS.contains(&hook_name.as_str()) {
        eprintln!("error: unknown hook event '{hook_name}';");
        eprintln!("use --allow-unknown-hook-name to allow non-native hook names");
        return Err(GitError::Exit(1));
    }
    let stdin = stdin_path.map(fs::read).transpose()?;
    run_hook(
        &hook_name,
        HookRun {
            args: hook_args,
            stdin,
            env: Vec::new(),
            stdout_to_stderr: true,
            error_if_missing: !ignore_missing,
            cwd: None,
            git_dir: None,
            normalize_failure: false,
            jobs,
            force_serial: false,
        },
        hook_env,
    )?;
    Ok(())
}

fn parse_hook_jobs_arg(value: &str) -> Result<usize> {
    let Ok(parsed) = value.parse::<isize>() else {
        eprintln!(
            "fatal: invalid value for -j: {value} (use -1 for CPU count or a positive integer)"
        );
        return Err(GitError::Exit(128));
    };
    if parsed == -1 {
        Ok(online_cpus())
    } else if parsed > 0 {
        Ok(parsed as usize)
    } else {
        eprintln!(
            "fatal: invalid value for -j: {parsed} (use -1 for CPU count or a positive integer)"
        );
        Err(GitError::Exit(128))
    }
}

fn list_hook_commands(hook_name: &str, hook_env: &HookEnvironment) -> Result<Vec<HookCommand>> {
    let config = hook_config(hook_env);
    list_hook_commands_with_config(hook_name, hook_env, &config)
}

fn list_hook_commands_with_config(
    hook_name: &str,
    hook_env: &HookEnvironment,
    config: &[ScopedSection],
) -> Result<Vec<HookCommand>> {
    let mut hooks = Vec::new();
    for hook in configured_hooks(config, hook_name)? {
        hooks.push(hook);
    }
    if let Some(path) = find_hook(config, hook_name, hook_env)? {
        hooks.push(HookCommand::Traditional(path));
    }
    Ok(hooks)
}

fn resolve_hook_git_dir(hook_env: &HookEnvironment) -> Option<PathBuf> {
    if let Some(git_dir) = hook_env.git_dir.as_ref() {
        return Some(git_dir.clone());
    }
    let cwd = env::current_dir().ok()?;
    discover_git_dir_respecting_environment(&cwd).ok()
}

fn hook_config(hook_env: &HookEnvironment) -> Vec<ScopedSection> {
    let common_git_dir =
        resolve_hook_git_dir(hook_env).map(|git_dir| repository_common_dir(&git_dir));
    let context = sley_config::ConfigIncludeContext::new(common_git_dir.clone(), None);
    let mut out = Vec::new();
    if let Ok(config) = sley_config::load_pre_dispatch_config(None, &context) {
        out.extend(config.sections.into_iter().map(|section| ScopedSection {
            scope: "global",
            section,
        }));
    }
    if let Some(common_git_dir) = common_git_dir.as_deref()
        && let Ok(config) =
            sley_config::load_config_with_includes(&common_git_dir.join("config"), &context)
    {
        out.extend(config.sections.into_iter().map(|section| ScopedSection {
            scope: "local",
            section,
        }));
    }
    let injected_from_env = sley_config::injected_config_parameters(None).ok();
    let parameters = hook_env
        .injected_config
        .as_ref()
        .or(injected_from_env.as_ref());
    if let Some(parameters) = parameters {
        out.extend(
            sley_config::injected_config_sections(parameters)
                .into_iter()
                .map(|section| ScopedSection {
                    scope: "command",
                    section,
                }),
        );
    }
    out
}

#[derive(Default)]
struct HookConfigState {
    commands: HashMap<String, String>,
    event_hooks: Vec<(String, Vec<EventHookEntry>)>,
    disabled_names: Vec<String>,
    parallel: HashMap<String, bool>,
    global_jobs: Option<usize>,
    event_jobs: HashMap<String, usize>,
}

#[derive(Clone)]
struct EventHookEntry {
    name: String,
    scope: &'static str,
}

fn configured_hooks(config: &[ScopedSection], hook_name: &str) -> Result<Vec<HookCommand>> {
    let state = parse_hook_config(config)?;
    let friendly_names = friendly_name_set(&state);
    let event_disabled =
        name_is_disabled(&state.disabled_names, hook_name) && !friendly_names.contains(hook_name);
    warn_jobs_on_friendly_names(&state, &friendly_names);
    let mut out = Vec::new();
    let Some((_, hooks)) = state
        .event_hooks
        .iter()
        .find(|(event, _)| event == hook_name)
    else {
        return Ok(out);
    };
    for event_hook in hooks {
        let name = &event_hook.name;
        let disabled = name_is_disabled(&state.disabled_names, name);
        let command = match state.commands.get(name) {
            Some(command) => command.clone(),
            None if disabled => {
                eprintln!("warning: disabled hook '{name}' has no command configured");
                String::new()
            }
            None => {
                eprintln!(
                    "fatal: 'hook.{name}.command' must be configured or 'hook.{name}.event' must be removed; aborting."
                );
                return Err(GitError::Exit(128));
            }
        };
        out.push(HookCommand::Configured {
            name: name.clone(),
            command,
            disabled,
            event_disabled,
            parallel: state.parallel.get(name).copied().unwrap_or(false),
            scope: event_hook.scope,
        });
    }
    Ok(out)
}

fn parse_hook_config(config: &[ScopedSection]) -> Result<HookConfigState> {
    let mut state = HookConfigState::default();
    for scoped in config {
        let section = &scoped.section;
        if !section.name.eq_ignore_ascii_case("hook") {
            continue;
        }
        for entry in &section.entries {
            let Some(name) = section.subsection.as_deref() else {
                if entry.key.eq_ignore_ascii_case("jobs")
                    && let Some(value) = entry.value.as_deref()
                    && let Some(jobs) = parse_hook_jobs_config("hook.jobs", value)
                {
                    state.global_jobs = Some(jobs);
                }
                continue;
            };
            if entry.key.eq_ignore_ascii_case("event") {
                let Some(value) = entry.value.as_deref() else {
                    continue;
                };
                if value.is_empty() {
                    remove_event_hook_from_all(&mut state.event_hooks, name);
                    continue;
                }
                if KNOWN_HOOKS.contains(&name) {
                    eprintln!(
                        "fatal: hook friendly-name '{name}' collides with a known event name; please choose a different friendly-name"
                    );
                    return Err(GitError::Exit(128));
                }
                if name == value {
                    eprintln!(
                        "warning: hook friendly-name '{name}' is the same as its event; this may cause ambiguity with hook.{name}.enabled"
                    );
                }
                push_event_hook(&mut state.event_hooks, value, name, scoped.scope);
            } else if entry.key.eq_ignore_ascii_case("command") {
                if let Some(value) = entry.value.clone() {
                    state.commands.insert(name.to_string(), value);
                }
            } else if entry.key.eq_ignore_ascii_case("enabled") {
                match entry
                    .value
                    .as_deref()
                    .and_then(sley_config::parse_config_bool)
                {
                    Some(false) => add_disabled_name(&mut state.disabled_names, name),
                    Some(true) => remove_disabled_name(&mut state.disabled_names, name),
                    None => {}
                }
            } else if entry.key.eq_ignore_ascii_case("parallel") {
                if let Some(value) = entry.value.as_deref() {
                    if let Some(value) = sley_config::parse_config_bool(value) {
                        state.parallel.insert(name.to_string(), value);
                    } else {
                        eprintln!(
                            "warning: hook.{name}.parallel must be a boolean, ignoring: '{value}'"
                        );
                    }
                }
            } else if entry.key.eq_ignore_ascii_case("jobs")
                && let Some(value) = entry.value.as_deref()
                && let Some(jobs) = parse_hook_jobs_config(&format!("hook.{name}.jobs"), value)
            {
                state.event_jobs.insert(name.to_string(), jobs);
            }
        }
    }
    Ok(state)
}

fn push_event_hook(
    event_hooks: &mut Vec<(String, Vec<EventHookEntry>)>,
    event: &str,
    name: &str,
    scope: &'static str,
) {
    let pos = match event_hooks
        .iter()
        .position(|(existing, _)| existing == event)
    {
        Some(pos) => pos,
        None => {
            event_hooks.push((event.to_string(), Vec::new()));
            event_hooks.len() - 1
        }
    };
    event_hooks[pos].1.retain(|entry| entry.name != name);
    event_hooks[pos].1.push(EventHookEntry {
        name: name.to_string(),
        scope,
    });
}

fn remove_event_hook_from_all(event_hooks: &mut Vec<(String, Vec<EventHookEntry>)>, name: &str) {
    for (_, hooks) in event_hooks {
        hooks.retain(|entry| entry.name != name);
    }
}

fn add_disabled_name(disabled_names: &mut Vec<String>, name: &str) {
    if !name_is_disabled(disabled_names, name) {
        disabled_names.push(name.to_string());
    }
}

fn remove_disabled_name(disabled_names: &mut Vec<String>, name: &str) {
    disabled_names.retain(|existing| existing != name);
}

fn name_is_disabled(disabled_names: &[String], name: &str) -> bool {
    disabled_names.iter().any(|existing| existing == name)
}

fn friendly_name_set(state: &HookConfigState) -> HashSet<String> {
    let mut names = HashSet::new();
    names.extend(state.commands.keys().cloned());
    names.extend(state.parallel.keys().cloned());
    for (_, hooks) in &state.event_hooks {
        names.extend(hooks.iter().map(|entry| entry.name.clone()));
    }
    names
}

fn warn_jobs_on_friendly_names(state: &HookConfigState, friendly_names: &HashSet<String>) {
    for name in state.event_jobs.keys() {
        if friendly_names.contains(name) {
            eprintln!(
                "warning: hook.{name}.jobs is set but '{name}' looks like a hook friendly-name, not an event name; hook.<event>.jobs uses the event name (e.g. hook.post-receive.jobs), so this setting will be ignored"
            );
        }
    }
}

fn parse_hook_jobs_config(key: &str, value: &str) -> Option<usize> {
    match sley_config::parse_config_int(value) {
        Some(-1) => Some(online_cpus()),
        Some(v) if v > 0 => Some(v as usize),
        Some(v) => {
            eprintln!("warning: {key} must be a positive integer or -1, ignoring: {v}");
            None
        }
        None => {
            eprintln!("warning: {key} must be an integer, ignoring: '{value}'");
            None
        }
    }
}

fn find_hook(
    config: &[ScopedSection],
    hook_name: &str,
    hook_env: &HookEnvironment,
) -> Result<Option<PathBuf>> {
    let Some(git_dir) = resolve_hook_git_dir(hook_env) else {
        return Ok(None);
    };
    let common_git_dir = repository_common_dir(&git_dir);
    let hooks_path = scoped_config_get(config, "core", None, "hooksPath");
    let hook_dir = hooks_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| common_git_dir.join("hooks"));
    let hook_dir = if hook_dir.is_absolute() {
        hook_dir
    } else if hooks_path.is_some() {
        env::current_dir()?.join(hook_dir)
    } else {
        hook_dir
    };
    let path = hook_dir.join(hook_name);
    if is_executable_file(&path) {
        Ok(Some(path))
    } else if path.is_file() {
        advise_ignored_hook(&path, config);
        Ok(None)
    } else {
        Ok(None)
    }
}

fn scoped_config_get(
    config: &[ScopedSection],
    section: &str,
    subsection: Option<&str>,
    key: &str,
) -> Option<String> {
    config
        .iter()
        .rev()
        .filter(|candidate| {
            candidate.section.name.eq_ignore_ascii_case(section)
                && candidate.section.subsection.as_deref() == subsection
        })
        .flat_map(|candidate| candidate.section.entries.iter().rev())
        .find(|entry| entry.key.eq_ignore_ascii_case(key))
        .and_then(|entry| entry.value.clone())
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn hook_failure_code(code: Option<i32>, options: &HookRun) -> i32 {
    if options.normalize_failure {
        1
    } else {
        code.unwrap_or(1)
    }
}

fn resolve_hook_jobs(
    hook_name: &str,
    options: &HookRun,
    config: &[ScopedSection],
    hooks: &[HookCommand],
) -> usize {
    if let Some(jobs) = options.jobs {
        warn_non_parallel_hooks_override(jobs, hooks);
        return jobs.max(1);
    }
    if options.force_serial {
        return 1;
    }
    let mut jobs = 1;
    if let Ok(state) = parse_hook_config(config) {
        if let Some(config_jobs) = state.global_jobs {
            jobs = config_jobs;
        }
        if let Some(event_jobs) = state.event_jobs.get(hook_name) {
            jobs = *event_jobs;
        }
    }
    if hooks.iter().any(|hook| {
        matches!(
            hook,
            HookCommand::Configured {
                parallel: false,
                ..
            }
        )
    }) {
        jobs = 1;
    }
    jobs.max(1)
}

fn warn_non_parallel_hooks_override(jobs: usize, hooks: &[HookCommand]) {
    if jobs <= 1 {
        return;
    }
    for hook in hooks {
        if let HookCommand::Configured {
            name,
            parallel: false,
            ..
        } = hook
        {
            eprintln!(
                "warning: hook '{name}' is not marked as parallel=true, running in parallel anyway due to -j{jobs}"
            );
        }
    }
}

fn online_cpus() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

fn trace_hook_region(hook_name: &str, jobs: usize) {
    let Some(target) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let target = target.to_string_lossy();
    if !target.starts_with('/') {
        return;
    }
    let label = escape_json(hook_name);
    let msg = escape_json(&format!("max:{jobs}"));
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(target.as_ref())
    {
        let _ = writeln!(
            file,
            "{{\"event\":\"region_enter\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"hook\",\"label\":\"{label}\",\"msg\":\"{msg}\"}}"
        );
        let _ = writeln!(
            file,
            "{{\"event\":\"region_leave\",\"sid\":\"sley\",\"thread\":\"main\",\"nesting\":1,\"category\":\"hook\",\"label\":\"{label}\"}}"
        );
    }
}

fn escape_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

fn advise_ignored_hook(path: &Path, config: &[ScopedSection]) {
    let advice_enabled = scoped_config_get(config, "advice", None, "ignoredHook")
        .and_then(|value| sley_config::parse_config_bool(&value))
        .unwrap_or(true);
    if !advice_enabled {
        return;
    }
    eprintln!(
        "hint: The '{}' hook was ignored because it's not set as executable.",
        path.display()
    );
    eprintln!("hint: You can disable this warning with `git config advice.ignoredHook false`.");
}

fn default_hook_cwd() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    let git_dir = discover_git_dir_respecting_environment(&cwd).ok()?;
    hook_cwd_for_git_dir(&git_dir).ok()
}

fn hook_cwd_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    if let Some(work_tree) = environment_work_tree() {
        let cwd = env::current_dir().map_err(|err| GitError::Io(err.to_string()))?;
        let resolved = resolve_path_from_cwd(&cwd, &work_tree);
        return fs::canonicalize(resolved)
            .map_err(|err| GitError::Io(err.to_string()))
            .or_else(|_| Ok(resolve_path_from_cwd(&cwd, &work_tree)));
    }
    match worktree_root_for_git_dir(git_dir) {
        Ok(Some(root)) => Ok(root),
        Ok(None) => Ok(git_dir.to_path_buf()),
        Err(_) => Ok(git_dir.to_path_buf()),
    }
}

fn hook_git_prefix(git_dir: &Path) -> Option<String> {
    let root = hook_cwd_for_git_dir(git_dir).ok()?;
    let root = fs::canonicalize(root).ok()?;
    let cwd = fs::canonicalize(env::current_dir().ok()?).ok()?;
    let relative = cwd.strip_prefix(&root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let mut prefix = relative.to_string_lossy().replace('\\', "/");
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    Some(prefix)
}

fn run_hooks_parallel(
    hook_name: &str,
    hooks: &[HookCommand],
    options: &mut HookRun,
    jobs: usize,
) -> Result<bool> {
    options.stdout_to_stderr = true;
    trace_hook_region(hook_name, jobs);
    let mut first_failure = None;
    let chunk_size = jobs.max(1);
    for chunk in hooks.chunks(chunk_size) {
        let mut children = Vec::with_capacity(chunk.len());
        for hook in chunk {
            children.push(spawn_hook_child(
                hook,
                options,
                Stdio::piped(),
                Stdio::piped(),
            )?);
        }
        for running in children {
            let output = running.child.wait_with_output()?;
            let mut merged = output.stdout;
            merged.extend_from_slice(&output.stderr);
            let status = output.status;
            if !merged.is_empty() {
                io::stderr().write_all(&merged)?;
                io::stderr().flush()?;
            }
            if !status.success() && first_failure.is_none() {
                first_failure = Some(status.code());
            }
        }
    }
    if let Some(code) = first_failure {
        return Err(GitError::Exit(hook_failure_code(code, options)));
    }
    Ok(true)
}

struct RunningHook {
    child: Child,
}

fn spawn_hook(hook: &HookCommand, options: &HookRun) -> Result<ExitStatus> {
    let stdout = if options.stdout_to_stderr {
        Stdio::piped()
    } else {
        Stdio::inherit()
    };
    let running = spawn_hook_child(hook, options, stdout, Stdio::inherit())?;
    let output = running.child.wait_with_output()?;
    if options.stdout_to_stderr {
        io::stderr().write_all(&output.stdout)?;
        io::stderr().flush()?;
    }
    Ok(output.status)
}

fn spawn_hook_child(
    hook: &HookCommand,
    options: &HookRun,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<RunningHook> {
    let cwd = options.cwd.clone().or_else(default_hook_cwd);
    let mut command = match hook {
        HookCommand::Traditional(path) => {
            let program = cwd
                .as_ref()
                .and_then(|cwd| path.strip_prefix(cwd).ok())
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| path.clone());
            Command::new(program)
        }
        HookCommand::Configured { command, .. } => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    if let Some(git_dir) = &options.git_dir {
        command.env("GIT_DIR", git_dir);
        if let Some(prefix) = hook_git_prefix(git_dir) {
            command.env("GIT_PREFIX", prefix);
        }
    }
    command.args(&options.args);
    for (key, value) in &options.env {
        command.env(key, value);
    }
    if options.stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    command.stdout(stdout);
    command.stderr(stderr);
    let mut child = command.spawn().map_err(|err| {
        eprintln!("fatal: cannot spawn {}: {err}", hook.display_name());
        GitError::Exit(1)
    })?;
    if let Some(input) = &options.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        match stdin.write_all(input) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(RunningHook { child })
}

impl HookCommand {
    fn display_name(&self) -> String {
        match self {
            HookCommand::Traditional(path) => path.display().to_string(),
            HookCommand::Configured { command, .. } => command.clone(),
        }
    }
}

fn hook_usage() {
    eprintln!(
        "usage: git hook run [--allow-unknown-hook-name] [--ignore-missing] [--to-stdin=<path>] <hook-name> [-- <hook-args>]"
    );
    eprintln!("   or: git hook list [--allow-unknown-hook-name] [-z] [--show-scope] <hook-name>");
}

fn hook_run_usage() {
    eprintln!(
        "usage: git hook run [--allow-unknown-hook-name] [--ignore-missing] [--to-stdin=<path>] <hook-name> [-- <hook-args>]"
    );
}

fn hook_list_usage() {
    eprintln!("usage: git hook list [--allow-unknown-hook-name] [-z] [--show-scope] <hook-name>");
}
