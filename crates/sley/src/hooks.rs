//! Git hook discovery and execution for embedders and the CLI.
//!
//! Traditional `$GIT_DIR/hooks/<name>` scripts and configured `hook.*` commands
//! from git config are both supported. Callers that inject `-c` / `GIT_CONFIG_*`
//! overrides (as the CLI does) should supply them via [`HookEnvironment`].

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sley_config::{ConfigParameter, ConfigSection};
use sley_core::GitError;
use sley_odb::repository_common_dir;
use sley_worktree::worktree_root_for_git_dir;

use crate::open_env::{discover_git_dir_respecting_environment, environment_work_tree, resolve_path_from_cwd};
use crate::Result;

#[derive(Clone)]
enum HookCommand {
    Traditional(PathBuf),
    Configured {
        name: String,
        command: String,
        disabled: bool,
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
    match args.first().map(String::as_str) {
        Some("list") => cmd_hook_list(&args[1..], &HookEnvironment::from_process()),
        Some("run") => cmd_hook_run(&args[1..], &HookEnvironment::from_process()),
        _ => {
            hook_usage();
            Err(GitError::Exit(129))
        }
    }
}

/// Run all hooks registered for `hook_name`.
pub fn run_hook(hook_name: &str, options: HookRun, hook_env: &HookEnvironment) -> Result<bool> {
    let hooks = list_hook_commands(hook_name, hook_env)?;
    let runnable = hooks
        .into_iter()
        .filter(|hook| !matches!(hook, HookCommand::Configured { disabled: true, .. }))
        .collect::<Vec<_>>();
    if runnable.is_empty() {
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
    for hook in runnable {
        let status = spawn_hook(&hook, &options)?;
        if !status.success() {
            return Err(GitError::Exit(hook_failure_code(status.code(), &options)));
        }
    }
    Ok(true)
}

/// Convenience wrapper around [`run_hook`] with string-slice arguments.
pub fn run_hook_l(
    hook_name: &str,
    args: &[&str],
    hook_env: &HookEnvironment,
) -> Result<bool> {
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
    Ok(list_hook_commands(hook_name, hook_env)?
        .into_iter()
        .any(|hook| !matches!(hook, HookCommand::Configured { disabled: true, .. })))
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
    };
    let status = spawn_hook(&HookCommand::Traditional(path), &options)?;
    Ok(!status.success())
}

/// Run a traditional hook script at `$GIT_DIR/hooks/<hook_name>`.
pub fn run_traditional_hook_at(
    git_dir: &Path,
    hook_name: &str,
    options: HookRun,
) -> Result<bool> {
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
                scope,
                ..
            } => {
                if show_scope && disabled {
                    write!(stdout, "{scope}\tdisabled\t{name}{terminator}")?;
                } else if show_scope {
                    write!(stdout, "{scope}\t{name}{terminator}")?;
                } else if disabled {
                    write!(stdout, "disabled\t{name}{terminator}")?;
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
        },
        hook_env,
    )?;
    Ok(())
}

fn list_hook_commands(hook_name: &str, hook_env: &HookEnvironment) -> Result<Vec<HookCommand>> {
    let mut hooks = Vec::new();
    let config = hook_config(hook_env);
    for hook in configured_hooks(&config, hook_name)? {
        hooks.push(hook);
    }
    if let Some(path) = find_hook(&config, hook_name, hook_env)? {
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
    let common_git_dir = resolve_hook_git_dir(hook_env).map(|git_dir| repository_common_dir(&git_dir));
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

fn configured_hooks(config: &[ScopedSection], hook_name: &str) -> Result<Vec<HookCommand>> {
    #[derive(Default)]
    struct State {
        events: Vec<String>,
        command: Option<String>,
        disabled: bool,
        scope: &'static str,
    }
    let mut states: Vec<(String, State)> = Vec::new();
    let state_for = |states: &mut Vec<(String, State)>, name: &str| -> usize {
        if let Some(pos) = states.iter().position(|(existing, _)| existing == name) {
            pos
        } else {
            states.push((name.to_string(), State::default()));
            states.len() - 1
        }
    };
    for scoped in config {
        let section = &scoped.section;
        if !section.name.eq_ignore_ascii_case("hook") {
            continue;
        }
        let Some(name) = section.subsection.as_deref() else {
            continue;
        };
        for entry in &section.entries {
            if entry.key.eq_ignore_ascii_case("event") {
                if let Some(value) = &entry.value {
                    let mut pos = state_for(&mut states, name);
                    if value.is_empty() {
                        states[pos].1.events.clear();
                    } else {
                        states[pos].1.events.retain(|existing| existing != value);
                        if states[pos].1.events.iter().any(|event| event == value)
                            || states[pos].1.command.is_some()
                        {
                            let item = states.remove(pos);
                            states.push(item);
                            pos = states.len() - 1;
                        }
                        states[pos].1.events.push(value.clone());
                    }
                    states[pos].1.scope = scoped.scope;
                }
            } else if entry.key.eq_ignore_ascii_case("command") {
                let pos = state_for(&mut states, name);
                let state = &mut states[pos].1;
                state.command = entry.value.clone();
                state.scope = scoped.scope;
            } else if entry.key.eq_ignore_ascii_case("enabled") {
                let pos = state_for(&mut states, name);
                let state = &mut states[pos].1;
                match entry
                    .value
                    .as_deref()
                    .and_then(sley_config::parse_config_bool)
                {
                    Some(false) => state.disabled = true,
                    Some(true) => state.disabled = false,
                    None => {}
                }
                state.scope = scoped.scope;
            }
        }
    }
    let mut out = Vec::new();
    for (name, state) in states {
        if state.events.iter().any(|event| event == hook_name) {
            let command = match state.command {
                Some(command) => command,
                None if state.disabled => {
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
                name,
                command,
                disabled: state.disabled,
                scope: state.scope,
            });
        }
    }
    Ok(out)
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

fn spawn_hook(hook: &HookCommand, options: &HookRun) -> Result<std::process::ExitStatus> {
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
    if options.stdout_to_stderr {
        command.stdout(Stdio::piped());
    }
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
    let output = child.wait_with_output()?;
    if options.stdout_to_stderr {
        io::stderr().write_all(&output.stdout)?;
        io::stderr().flush()?;
    }
    Ok(output.status)
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