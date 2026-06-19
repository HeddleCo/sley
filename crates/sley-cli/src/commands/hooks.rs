use crate::*;
use std::process::{Command, Stdio};

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

pub(crate) struct HookRun {
    pub(crate) args: Vec<String>,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) stdout_to_stderr: bool,
    pub(crate) error_if_missing: bool,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) git_dir: Option<PathBuf>,
    pub(crate) normalize_failure: bool,
}

impl Default for HookRun {
    fn default() -> Self {
        Self {
            args: Vec::new(),
            stdin: None,
            stdout_to_stderr: true,
            error_if_missing: false,
            cwd: None,
            git_dir: None,
            normalize_failure: true,
        }
    }
}

pub(crate) const KNOWN_HOOKS: &[&str] = &[
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

pub(crate) fn cmd_hook(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => cmd_hook_list(&args[1..]),
        Some("run") => cmd_hook_run(&args[1..]),
        _ => {
            hook_usage();
            Err(GitError::Exit(129))
        }
    }
}

pub(crate) fn run_hook(hook_name: &str, options: HookRun) -> Result<bool> {
    let hooks = list_hook_commands(hook_name)?;
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
    if options.git_dir.is_none()
        && let Ok(cwd) = env::current_dir()
        && let Ok(git_dir) = discover_git_dir(cwd)
    {
        options.git_dir = Some(git_dir);
    }
    for hook in runnable {
        let status = spawn_hook(&hook, &options)?;
        if !status.success() {
            return Err(GitError::Exit(hook_failure_code(status.code(), &options)));
        }
    }
    Ok(true)
}

pub(crate) fn run_hook_l(hook_name: &str, args: &[&str]) -> Result<bool> {
    run_hook(
        hook_name,
        HookRun {
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            ..HookRun::default()
        },
    )
}

pub(crate) fn hook_exists(hook_name: &str) -> Result<bool> {
    Ok(list_hook_commands(hook_name)?
        .into_iter()
        .any(|hook| !matches!(hook, HookCommand::Configured { disabled: true, .. })))
}

/// Run the traditional `$GIT_DIR/hooks/reference-transaction` hook for one
/// phase, feeding the queued `<old> <new> <refname>` lines on stdin. Returns
/// `Ok(true)` when the hook ran and exited nonzero (the caller decides whether
/// that aborts the transaction — only the `preparing`/`prepared` phases do),
/// `Ok(false)` when the hook is absent or exited zero, and `Err` only on a spawn
/// I/O failure. Unlike [`run_traditional_hook_at`], a nonzero exit is reported
/// rather than turned into a fatal, because git ignores it in the
/// `committed`/`aborted` phases.
pub(crate) fn run_reference_transaction_hook_at(
    git_dir: &Path,
    phase: &str,
    stdin: Vec<u8>,
) -> Result<bool> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let path = common_git_dir.join("hooks").join("reference-transaction");
    if !is_executable_file(&path) {
        return Ok(false);
    }
    let options = HookRun {
        args: vec![phase.to_string()],
        stdin: Some(stdin),
        stdout_to_stderr: false,
        error_if_missing: false,
        cwd: Some(hook_cwd_for_git_dir(git_dir)?),
        git_dir: Some(git_dir.to_path_buf()),
        normalize_failure: false,
    };
    let status = spawn_hook(&HookCommand::Traditional(path), &options)?;
    Ok(!status.success())
}

pub(crate) fn run_traditional_hook_at(
    git_dir: &Path,
    hook_name: &str,
    options: HookRun,
) -> Result<bool> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
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

fn cmd_hook_list(args: &[String]) -> Result<()> {
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
    let hooks = list_hook_commands(&hook_name)?;
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

fn cmd_hook_run(args: &[String]) -> Result<()> {
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
            stdout_to_stderr: true,
            error_if_missing: !ignore_missing,
            cwd: None,
            git_dir: None,
            normalize_failure: false,
        },
    )?;
    Ok(())
}

fn list_hook_commands(hook_name: &str) -> Result<Vec<HookCommand>> {
    let mut hooks = Vec::new();
    let config = hook_config();
    for hook in configured_hooks(&config, hook_name)? {
        hooks.push(hook);
    }
    if let Some(path) = find_hook(&config, hook_name)? {
        hooks.push(HookCommand::Traditional(path));
    }
    Ok(hooks)
}

fn hook_config() -> Vec<ScopedSection> {
    let common_git_dir =
        discover_git_dir(env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
            .ok()
            .and_then(|git_dir| common_git_dir_for_git_dir(&git_dir).ok());
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
    if let Ok(parameters) = crate::injected_config_parameters() {
        out.extend(
            sley_config::injected_config_sections(&parameters)
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

fn find_hook(config: &[ScopedSection], hook_name: &str) -> Result<Option<PathBuf>> {
    let git_dir = match discover_git_dir(env::current_dir()?) {
        Ok(git_dir) => git_dir,
        Err(_) => return Ok(None),
    };
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
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
    let git_dir = discover_git_dir(env::current_dir().ok()?).ok()?;
    hook_cwd_for_git_dir(&git_dir).ok()
}

fn hook_cwd_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    match worktree_root_for_git_dir(git_dir) {
        Ok(root) => Ok(root),
        Err(_) => Ok(git_dir.to_path_buf()),
    }
}

fn spawn_hook(hook: &HookCommand, options: &HookRun) -> Result<std::process::ExitStatus> {
    let mut command = match hook {
        HookCommand::Traditional(path) => Command::new(path),
        HookCommand::Configured { command, .. } => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    if let Some(cwd) = options.cwd.clone().or_else(default_hook_cwd) {
        command.current_dir(cwd);
    }
    if let Some(git_dir) = &options.git_dir {
        command.env("GIT_DIR", git_dir);
    }
    command.args(&options.args);
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
