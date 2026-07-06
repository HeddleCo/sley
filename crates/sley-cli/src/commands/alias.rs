//! Git-style command alias resolution before dispatch.
//!
//! Mirrors upstream git's `alias.c` (`config_alias_cb` / `alias_lookup` /
//! `list_aliases`) and the alias-expansion half of `git.c`'s `run_argv` /
//! `handle_alias`: non-built-in (and *deprecated* built-in) command names are
//! looked up in the `alias.*` config namespace, expanded (simple or shell `!`
//! aliases), and re-dispatched with loop detection.
//!
//! Two config syntaxes are honoured, exactly as `config_alias_cb`:
//! - `alias.<name> = <value>` — no subsection, case-*insensitive* name.
//! - `[alias "<name>"] command = <value>` — subsection, case-*sensitive* name.
//!   An empty subsection (`alias..<key>`) is treated as the plain form.

use sley_config::{ConfigIncludeContext, GitConfig};
use sley_core::{GitError, Result};
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::commands::remote_cmds::repo_current_branch_name;
use crate::{
    common_git_dir_for_git_dir, injected_config_parameters,
    worktree_root_for_git_dir,
};

/// A safety backstop on alias-expansion iterations. git relies purely on its
/// loop/recursion detection; this guards against a pathological config that
/// expands forever without repeating a command name (e.g. an alias whose body
/// is only global options).
pub(crate) const MAX_ALIAS_DEPTH: usize = 1024;

/// Deprecated built-in command names. Like git, these may be *overridden* by an
/// alias (their alias lookup runs before built-in dispatch), giving a forward
/// path for users who keep typing the old name.
const DEPRECATED_COMMANDS: &[&str] = &["pack-redundant", "whatchanged"];

/// Whether `command` is a deprecated built-in (alias-overridable).
pub(crate) fn is_deprecated_command(command: &str) -> bool {
    DEPRECATED_COMMANDS.contains(&command)
}

/// Whether `command` is a built-in git command (built-ins are never aliased,
/// except the deprecated ones — see [`is_deprecated_command`]).
pub(crate) fn is_builtin_command(command: &str) -> bool {
    crate::commands::help::is_builtin_command(command)
}

/// The result of looking up a command name in the `alias.*` namespace, mirroring
/// `alias_lookup`'s `config_alias_cb`.
pub(crate) enum AliasLookup {
    /// No alias is defined for this name.
    None,
    /// An alias is defined with this value string.
    Value(String),
    /// An alias key matched but its value is a bare boolean (missing value);
    /// carries the full config key for git's `missing value for '<key>'` error.
    MissingValue(String),
}

/// Look up `command` against the `alias.*` config, faithfully reproducing git's
/// `config_alias_cb`: the *last* matching entry wins; a matched entry with no
/// value is a hard error.
pub(crate) fn alias_lookup(command: &str) -> Result<AliasLookup> {
    let config = load_alias_config()?;
    let mut found: Option<String> = None;
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("alias") {
            continue;
        }
        // Treat `[alias ""]` (empty subsection) the same as plain `[alias]`.
        let subsection = section
            .subsection
            .as_deref()
            .filter(|name| !name.is_empty());
        for entry in &section.entries {
            // With a subsection, only the `command` key is an alias.
            if subsection.is_some() && !entry.key.eq_ignore_ascii_case("command") {
                continue;
            }
            let matches = match subsection {
                // Subsection name: byte-exact (case-sensitive).
                Some(name) => name == command,
                // Plain alias name: case-insensitive.
                None => entry.key.eq_ignore_ascii_case(command),
            };
            if !matches {
                continue;
            }
            match &entry.value {
                Some(value) => found = Some(value.clone()),
                None => {
                    // git's `git_config_string` on a NULL value aborts config
                    // parsing with `config_error_nonbool` — a fatal "missing
                    // value for '<key>'". The reported key is `alias.<name>` for
                    // the plain form, `alias.<subsection>.command` for the
                    // subsection form.
                    let key = match subsection {
                        Some(name) => format!("alias.{name}.command"),
                        None => format!("alias.{}", entry.key),
                    };
                    return Ok(AliasLookup::MissingValue(key));
                }
            }
        }
    }
    Ok(match found {
        Some(value) => AliasLookup::Value(value),
        None => AliasLookup::None,
    })
}

/// Every alias `(name, value)` defined in the effective config, for
/// `git help -a`. Mirrors `list_aliases`: plain entries contribute their key,
/// subsection entries contribute the subsection name (only for the `command`
/// key). The last value for a name wins, then names are sorted.
pub(crate) fn list_aliases() -> Result<Vec<(String, String)>> {
    let config = load_alias_config()?;
    let mut aliases: Vec<(String, String)> = Vec::new();
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("alias") {
            continue;
        }
        let subsection = section
            .subsection
            .as_deref()
            .filter(|name| !name.is_empty());
        for entry in &section.entries {
            let Some(value) = &entry.value else {
                continue;
            };
            let name = match subsection {
                Some(name) => {
                    if !entry.key.eq_ignore_ascii_case("command") {
                        continue;
                    }
                    name.to_string()
                }
                None => entry.key.clone(),
            };
            if let Some(existing) = aliases.iter_mut().find(|(n, _)| n == &name) {
                existing.1 = value.clone();
            } else {
                aliases.push((name, value.clone()));
            }
        }
    }
    aliases.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(aliases)
}

/// Load the effective config (system + global + repository) with the
/// command-line `-c` / `GIT_CONFIG_PARAMETERS` overrides folded in, so alias
/// lookups see `git -c alias.x=… x` just like file-defined aliases.
fn load_alias_config() -> Result<GitConfig> {
    let cwd = env::current_dir()?;
    let git_dir = crate::session::cli_git_dir_from(&cwd).ok();
    let common_git_dir = git_dir
        .as_ref()
        .and_then(|dir| common_git_dir_for_git_dir(dir).ok());
    let branch = git_dir
        .as_ref()
        .and_then(|dir| repo_current_branch_name(dir));
    let context = ConfigIncludeContext::new(common_git_dir.clone(), branch);
    let mut config = sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)?;
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        &cwd,
    )?;
    Ok(config)
}

/// Execute a `!`-prefixed alias through git's shell path, reproducing git's
/// `prepare_shell_cmd` argv (`sh -c '<body> "$@"' '<body>' <args…>`) so the
/// alias body sees its arguments as `$@`/`$1`/`$*`, and emitting the
/// `trace: start_command:` line git's `run_command` prints under `GIT_TRACE`.
pub(crate) fn run_shell_alias(command: &str, extra_args: &[String]) -> Result<()> {
    let shell = match env::var("GIT_SHELL_PATH") {
        Ok(shell) => shell,
        Err(_) => "/bin/sh".into(),
    };
    // git's prepare_shell_cmd: the script is `<body> "$@"` when there are extra
    // args (so positional parameters reach the body), else the bare body; the
    // body is then passed again as `$0`, followed by the extra args.
    let script = if extra_args.is_empty() {
        command.to_string()
    } else {
        format!("{command} \"$@\"")
    };
    let mut prepared: Vec<String> = Vec::with_capacity(4 + extra_args.len());
    prepared.push(shell.clone());
    prepared.push("-c".to_string());
    prepared.push(script);
    prepared.push(command.to_string());
    prepared.extend(extra_args.iter().cloned());

    // git's start_command traces the prepared argv (minus the ENOEXEC-fallback
    // SHELL_PATH it prepends): `trace: start_command: <shell> -c <script> …`.
    if crate::setup::git_trace_enabled() {
        let mut line = String::from("trace: start_command:");
        for arg in &prepared {
            line.push(' ');
            line.push_str(&crate::setup::trace_quote_sq(arg));
        }
        crate::setup::git_trace_line("run-command.c:764", &line);
    }

    let mut process = ProcessCommand::new(&prepared[0]);
    process.args(&prepared[1..]);
    process.env(
        "SLEY_TRACE2_DEPTH",
        (sley_core::trace2::depth() + 1).to_string(),
    );
    configure_shell_alias_worktree_env(&mut process)?;
    // Propagate the effective config-injection parameters (`-c` / `--config-env`
    // folded onto any inherited `GIT_CONFIG_PARAMETERS`) to the subprocess git,
    // exactly as upstream git does by mutating its own env before running the
    // shell alias.
    if let Some(params) = crate::effective_config_parameters_env() {
        process.env("GIT_CONFIG_PARAMETERS", params);
    }
    let status = process
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        let code = match status.code() {
            Some(code) => code,
            None => 1,
        };
        Err(GitError::Exit(code))
    }
}

fn configure_shell_alias_worktree_env(process: &mut ProcessCommand) -> Result<()> {
    let cwd = env::current_dir()?;
    let Ok(git_dir) = crate::session::cli_git_dir_from(&cwd) else {
        return Ok(());
    };
    let Ok(root) = worktree_root_for_git_dir(&git_dir) else {
        return Ok(());
    };
    let root = canonical_or_self(root);
    let cwd = canonical_or_self(cwd);
    let Ok(prefix_path) = cwd.strip_prefix(&root) else {
        return Ok(());
    };
    process.current_dir(&root);
    let prefix = git_prefix_from_path(prefix_path);
    if prefix.is_empty() {
        process.env_remove("GIT_PREFIX");
    } else {
        process.env("GIT_PREFIX", prefix);
    }
    Ok(())
}

fn canonical_or_self(path: PathBuf) -> PathBuf {
    match fs::canonicalize(&path) {
        Ok(canonical) => canonical,
        Err(_) => path,
    }
}

fn git_prefix_from_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", path.to_string_lossy().replace('\\', "/"))
    }
}

/// Split an alias value into words, respecting single- and double-quoted spans.
pub(crate) fn split_alias_value(value: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' => {
                if !current.is_empty() {
                    args.push(mem::take(&mut current));
                }
            }
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    current.push(c);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_alias_value_respects_quotes() {
        assert_eq!(
            split_alias_value("log --pretty=format:'%h %s'"),
            vec!["log", "--pretty=format:%h %s"]
        );
        assert_eq!(split_alias_value("status -s"), vec!["status", "-s"]);
    }

    #[test]
    fn builtin_commands_are_not_aliased() {
        assert!(is_builtin_command("init"));
        assert!(is_builtin_command("config"));
        assert!(!is_builtin_command("aliasedinit"));
        assert!(!is_builtin_command("hello-world"));
    }

    #[test]
    fn deprecated_commands_are_overridable() {
        assert!(is_deprecated_command("whatchanged"));
        assert!(is_deprecated_command("pack-redundant"));
        assert!(!is_deprecated_command("status"));
    }
}
