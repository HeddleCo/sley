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
//!
//! Historically git also accepts the "simple dotted" form
//! `alias.foo.bar = value` (written that way or as `[alias "foo"] bar = …`):
//! when a subsection-style key is not the dedicated `command` key, the full
//! remainder after `alias.` becomes the two-level alias name (`foo.bar`),
//! matched case-insensitively. See `config_alias_cb` in git's `alias.c`.

use crate::sley_config;
use sley::plumbing::sley_config::ConfigIncludeContext;
use sley::plumbing::sley_core;
use sley::{GitError, Result};
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::{injected_config_parameters, worktree_root_for_git_dir};

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

/// Diagnostic payload for git's `config_error_nonbool` on a bare alias key:
/// `error: missing value for '<key>'` followed by
/// `fatal: bad config line <N> in file <path>` when the entry came from a file.
#[derive(Debug, Clone)]
pub(crate) struct AliasMissingValue {
    /// Canonical config key (`alias.br`, `alias.noval.command`, …).
    pub key: String,
    /// Origin path as git would report it (e.g. `.git/config`), when known.
    pub file: Option<String>,
    /// 1-based physical source line, when known.
    pub line: Option<usize>,
}

/// The result of looking up a command name in the `alias.*` namespace, mirroring
/// `alias_lookup`'s `config_alias_cb`.
pub(crate) enum AliasLookup {
    /// No alias is defined for this name.
    None,
    /// An alias is defined with this value string.
    Value(String),
    /// An alias key matched but its value is a bare boolean (missing value).
    MissingValue(AliasMissingValue),
}

/// One resolved alias definition: the command name users type, the expansion
/// value, and the original config key (for diagnostics).
struct ResolvedAlias<'a> {
    /// Name used as a git subcommand (`st`, `simple.dotted`, …).
    name: String,
    /// Subsection-form names are case-sensitive; plain / dotted are not.
    case_sensitive: bool,
    /// Full config key (`alias.<…>`) as git reports it.
    config_key: String,
    value: Option<&'a str>,
    file: Option<&'a str>,
    line: Option<usize>,
}

/// Look up `command` against the `alias.*` config, faithfully reproducing git's
/// `config_alias_cb`: the *last* matching entry wins; a matched entry with no
/// value is a hard error.
pub(crate) fn alias_lookup(
    cli_session: &crate::session::CliSession,
    command: &str,
) -> Result<AliasLookup> {
    let stack = load_alias_stack(cli_session)?;
    let mut found: Option<String> = None;
    for entry in &stack.entries {
        let Some(resolved) = resolve_alias_entry(entry) else {
            continue;
        };
        let matches = if resolved.case_sensitive {
            resolved.name == command
        } else {
            resolved.name.eq_ignore_ascii_case(command)
        };
        if !matches {
            continue;
        }
        match resolved.value {
            Some(value) => found = Some(value.to_string()),
            None => {
                // git's `git_config_string` on a NULL value aborts config
                // parsing with `config_error_nonbool` — a fatal "missing
                // value for '<key>'" plus the bad-config-line trailer.
                return Ok(AliasLookup::MissingValue(AliasMissingValue {
                    key: resolved.config_key,
                    file: resolved.file.map(str::to_string),
                    line: resolved.line,
                }));
            }
        }
    }
    Ok(match found {
        Some(value) => AliasLookup::Value(value),
        None => AliasLookup::None,
    })
}

/// Every alias `(name, value)` defined in the effective config, for
/// `git help -a`. Mirrors `list_aliases`: subsection `command` keys contribute
/// the subsection name; any other subsection key falls back to the simple
/// dotted name (`subsection.key`); plain keys contribute themselves. The last
/// value for a name wins, then names are sorted.
pub(crate) fn list_aliases(
    cli_session: &crate::session::CliSession,
) -> Result<Vec<(String, String)>> {
    let stack = load_alias_stack(cli_session)?;
    let mut aliases: Vec<(String, String)> = Vec::new();
    for entry in &stack.entries {
        let Some(resolved) = resolve_alias_entry(entry) else {
            continue;
        };
        let Some(value) = resolved.value else {
            // git's list path also dies on bare booleans; for help -a we skip
            // incomplete entries so a partial config still lists the rest.
            continue;
        };
        // Listing uses the resolved name exactly as stored (case preserved for
        // both forms). Last definition wins for equal names.
        if let Some(existing) = aliases.iter_mut().find(|(n, _)| n == &resolved.name) {
            existing.1 = value.to_string();
        } else {
            aliases.push((resolved.name, value.to_string()));
        }
    }
    aliases.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(aliases)
}

/// Map one config stack entry in the `alias` section to its alias name, using
/// git's `config_alias_cb` two-syntax rules (including the simple dotted
/// fallback when the key is not the dedicated `command` key).
fn resolve_alias_entry(entry: &sley_config::ConfigStackEntry) -> Option<ResolvedAlias<'_>> {
    if !entry.section.eq_ignore_ascii_case("alias") {
        return None;
    }
    // Treat `[alias ""]` (empty subsection) the same as plain `[alias]`.
    let subsection = entry.subsection.as_deref().filter(|name| !name.is_empty());
    let key = entry.key.as_str();
    let config_key = match subsection {
        Some(name) => format!("alias.{name}.{key}"),
        None => format!("alias.{key}"),
    };
    let file = match entry.origin.kind {
        sley_config::ConfigOriginKind::File if !entry.origin.name.is_empty() => {
            Some(entry.origin.name.as_str())
        }
        _ => None,
    };
    let line = entry.line_number;
    let value = entry.value.as_deref();

    // git: if (subsection && strcmp(key, "command")) fall back to two-level.
    // Note: `command` is compared case-sensitively, just like git's strcmp.
    if let Some(name) = subsection {
        if key == "command" {
            return Some(ResolvedAlias {
                name: name.to_string(),
                case_sensitive: true,
                config_key,
                value,
                file,
                line,
            });
        }
        // Simple dotted form: `alias.foo.bar` / `[alias "foo"] bar = …`
        // becomes the two-level alias name `foo.bar` (case-insensitive).
        return Some(ResolvedAlias {
            name: format!("{name}.{key}"),
            case_sensitive: false,
            config_key,
            value,
            file,
            line,
        });
    }

    Some(ResolvedAlias {
        name: key.to_string(),
        case_sensitive: false,
        config_key,
        value,
        file,
        line,
    })
}

/// Load the effective config event stream (system + global + repository +
/// command-line `-c` / `GIT_CONFIG_PARAMETERS`) so alias lookups see the same
/// layers as git's `read_early_config`, with origin/line metadata for the
/// missing-value diagnostic.
fn load_alias_stack(cli_session: &crate::session::CliSession) -> Result<sley_config::ConfigStack> {
    let cwd = cli_session.cwd();
    let snapshot = match cli_session.repository_snapshot() {
        Ok(snapshot) => Some(snapshot),
        Err(GitError::NotFound(_)) => None,
        Err(err) => return Err(crate::report_config_setup_error(err)),
    };
    let common_git_dir = snapshot
        .as_ref()
        .map(|snapshot| snapshot.common_dir.clone());
    let branch = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.branch.clone());
    let context = ConfigIncludeContext::new(common_git_dir.clone(), branch);
    let mut stack = sley_config::ConfigStack::new();
    for (path, scope) in sley_config::default_config_layer_paths() {
        stack.push_file(&path, scope, true, &context)?;
    }
    if let Some(common) = common_git_dir.as_ref() {
        // Prefer a cwd-relative origin name (`.git/config`) so the missing-
        // value fatal matches git's `bad config line N in file .git/config`.
        let local = alias_config_display_path(cwd, common.join("config"));
        stack.push_file(&local, sley_config::ConfigScope::Local, true, &context)?;
    }
    let parameters = injected_config_parameters()?;
    stack.push_parameters_with_includes(&parameters, &context)?;
    Ok(stack)
}

/// Display a config path the way git reports it: relative to the working
/// directory when it lies underneath.
fn alias_config_display_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return relative.to_path_buf();
    }
    if let Ok(process_cwd) = env::current_dir()
        && let Ok(relative) = path.strip_prefix(&process_cwd)
    {
        return relative.to_path_buf();
    }
    path
}

/// Execute a `!`-prefixed alias through git's shell path, reproducing git's
/// `prepare_shell_cmd` argv (`sh -c '<body> "$@"' '<body>' <args…>`) so the
/// alias body sees its arguments as `$@`/`$1`/`$*`, and emitting the
/// `trace: start_command:` line git's `run_command` prints under `GIT_TRACE`.
pub(crate) fn run_shell_alias(
    cli_session: &crate::session::CliSession,
    command: &str,
    extra_args: &[String],
) -> Result<()> {
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
    configure_shell_alias_worktree_env(cli_session, &mut process)?;
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

fn configure_shell_alias_worktree_env(
    cli_session: &crate::session::CliSession,
    process: &mut ProcessCommand,
) -> Result<()> {
    let cwd = cli_session.cwd();
    let Ok(git_dir) = cli_session.git_dir() else {
        return Ok(());
    };
    let Ok(root) = worktree_root_for_git_dir(cli_session, &git_dir) else {
        return Ok(());
    };
    let root = canonical_or_self(root);
    let cwd = canonical_or_self(cwd.to_path_buf());
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

    #[test]
    fn resolve_alias_entry_simple_dotted_fallback() {
        let entry = sley_config::ConfigStackEntry {
            section: "alias".into(),
            subsection: Some("simple".into()),
            key: "dotted".into(),
            value: Some("!echo ran".into()),
            scope: sley_config::ConfigScope::Local,
            origin: sley_config::ConfigOrigin::file(".git/config"),
            included_from: None,
            line_number: Some(3),
        };
        let resolved = resolve_alias_entry(&entry).expect("alias entry");
        assert_eq!(resolved.name, "simple.dotted");
        assert!(!resolved.case_sensitive);
        assert_eq!(resolved.config_key, "alias.simple.dotted");
        assert_eq!(resolved.value, Some("!echo ran"));
    }

    #[test]
    fn resolve_alias_entry_subsection_command_is_case_sensitive() {
        let entry = sley_config::ConfigStackEntry {
            section: "alias".into(),
            subsection: Some("SubCase".into()),
            key: "command".into(),
            value: Some("!echo upper".into()),
            scope: sley_config::ConfigScope::Local,
            origin: sley_config::ConfigOrigin::file(".git/config"),
            included_from: None,
            line_number: Some(2),
        };
        let resolved = resolve_alias_entry(&entry).expect("alias entry");
        assert_eq!(resolved.name, "SubCase");
        assert!(resolved.case_sensitive);
        assert_eq!(resolved.config_key, "alias.SubCase.command");
    }

    #[test]
    fn resolve_alias_entry_plain_form() {
        let entry = sley_config::ConfigStackEntry {
            section: "alias".into(),
            subsection: None,
            key: "st".into(),
            value: Some("status".into()),
            scope: sley_config::ConfigScope::Local,
            origin: sley_config::ConfigOrigin::file(".git/config"),
            included_from: None,
            line_number: Some(2),
        };
        let resolved = resolve_alias_entry(&entry).expect("alias entry");
        assert_eq!(resolved.name, "st");
        assert!(!resolved.case_sensitive);
        assert_eq!(resolved.config_key, "alias.st");
    }
}
