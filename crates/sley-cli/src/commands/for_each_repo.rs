//! `git for-each-repo` — run a git command in each repository named by a config
//! key. A faithful port of upstream `builtin/for-each-repo.c`:
//!
//! - `--config=<key>` is required; its value is read as a *multi-valued* config
//!   key (each repository path is one value).
//! - For each value, git runs `git -C <path> <args>` as a subprocess with the
//!   repository-local environment sanitized, so the child rediscovers its own
//!   repo from `<path>`. `<path>` is `~`-interpolated first.
//! - `--keep-going` continues past a failing repository (returning non-zero
//!   overall); without it the first failure stops the loop and propagates.
//! - A syntactically bad `--config` key, or a key whose value is a bare boolean
//!   (no value), is a usage error (exit 129); an unset key runs nothing (exit 0).
#![allow(clippy::expect_used)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::sley_config;
use sley::plumbing::sley_config::ConfigIncludeContext;
use sley::{GitError, Result};

use crate::commands::remote::repo_current_branch_name;
use crate::{common_git_dir_for_git_dir, injected_config_parameters};

const USAGE: &str = "usage: git for-each-repo --config=<config> [--] <arguments>";

/// Repository-local environment variables that must not leak into the child
/// `git` invocations (git's `local_repo_env`): the child must discover its own
/// repository from the `-C <path>` we hand it. `GIT_CONFIG_PARAMETERS` is
/// deliberately *not* cleared — git keeps `-c`-style overrides for the child.
const LOCAL_REPO_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_INDEX_VERSION",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

pub(crate) fn cmd_for_each_repo(
    cli_session: &crate::session::CliSession,
    args: &[String],
) -> Result<()> {
    let mut config_key: Option<String> = None;
    let mut keep_going = false;
    let mut index = 0;

    // PARSE_OPT_STOP_AT_NON_OPTION: options are recognized until the first
    // non-option (or `--`), after which everything is the per-repo command.
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Err(GitError::Exit(129));
            }
            "--" => {
                index += 1;
                break;
            }
            "--keep-going" => {
                keep_going = true;
                index += 1;
            }
            "--no-keep-going" => {
                keep_going = false;
                index += 1;
            }
            value if value.starts_with("--config=") => {
                config_key = Some(value["--config=".len()..].to_string());
                index += 1;
            }
            "--config" => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(None);
                };
                config_key = Some(value.clone());
                index += 2;
            }
            value if value.starts_with('-') && value != "-" => {
                return usage_error(Some(&format!("unknown option `{}'", &value[1..])));
            }
            _ => break,
        }
    }

    let child_args = &args[index..];

    let Some(config_key) = config_key else {
        eprintln!("fatal: missing --config=<config>");
        return Err(GitError::Exit(128));
    };

    let values = match read_repo_paths(cli_session, &config_key)? {
        ConfigOutcome::BadKey => {
            return usage_error(Some(&format!("got bad config --config={config_key}")));
        }
        ConfigOutcome::MissingValue(key) => {
            eprintln!("error: missing value for '{key}'");
            return usage_error(Some(&format!("got bad config --config={config_key}")));
        }
        ConfigOutcome::Unset => return Ok(()),
        ConfigOutcome::Values(values) => values,
    };

    let mut result = 0;
    for path in values {
        let code = run_command_on_repo(&path, child_args)?;
        if code != 0 {
            if !keep_going {
                return Err(GitError::Exit(code));
            }
            result = 1;
        }
    }

    if result == 0 {
        Ok(())
    } else {
        Err(GitError::Exit(result))
    }
}

/// Print the usage block (with an optional leading `fatal:`/`error:` line) and
/// return git's parse-options usage exit code (129).
fn usage_error(message: Option<&str>) -> Result<()> {
    if let Some(message) = message {
        eprintln!("{USAGE}");
        eprintln!();
        eprintln!("    --config <config>     config key storing a list of repository paths");
        eprintln!("    --keep-going          keep going even if command fails in a repository");
        let _ = message;
    } else {
        eprintln!("{USAGE}");
    }
    Err(GitError::Exit(129))
}

/// The outcome of resolving the `--config` key against the effective config.
enum ConfigOutcome {
    /// The key is syntactically invalid.
    BadKey,
    /// The key exists but a value is a bare boolean (no value); carries the
    /// canonical key for the `missing value for '<key>'` diagnostic.
    MissingValue(String),
    /// The key is unset — run nothing.
    Unset,
    /// One or more repository paths.
    Values(Vec<String>),
}

/// Read every value of the multi-valued config key from the effective config
/// (system + global + repository + `-c`/`GIT_CONFIG_PARAMETERS` overrides).
fn read_repo_paths(
    cli_session: &crate::session::CliSession,
    key: &str,
) -> Result<ConfigOutcome> {
    let canonical = match sley_config::canonicalize_config_key(key) {
        Ok(canonical) => canonical,
        Err(_) => return Ok(ConfigOutcome::BadKey),
    };
    let (section, subsection, variable) = split_canonical_key(&canonical);

    let git_dir = cli_session.git_dir().ok();
    let common_git_dir = git_dir
        .as_ref()
        .and_then(|dir| common_git_dir_for_git_dir(dir).ok());
    let branch = git_dir
        .as_ref()
        .and_then(|dir| repo_current_branch_name(dir));
    let context = ConfigIncludeContext::new(common_git_dir.clone(), branch);

    let mut config = sley_config::load_pre_dispatch_config(common_git_dir.as_deref(), &context)
        .map_err(|err| GitError::Io(err.to_string()))?;
    let parameters = injected_config_parameters()?;
    sley_config::append_injected_config_sections_with_includes(
        &mut config,
        &parameters,
        &context,
        cli_session.cwd(),
    )
    .map_err(|err| GitError::Io(err.to_string()))?;

    let entries = config.get_all(section, subsection.as_deref(), variable);
    if entries.is_empty() {
        return Ok(ConfigOutcome::Unset);
    }
    let mut values = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            Some(value) => values.push(value.to_string()),
            None => return Ok(ConfigOutcome::MissingValue(canonical)),
        }
    }
    Ok(ConfigOutcome::Values(values))
}

/// Split a canonicalized config key (`section[.subsection].variable`) into its
/// parts. The section is everything before the first dot, the variable
/// everything after the last dot, and any text between is the subsection.
fn split_canonical_key(canonical: &str) -> (&str, Option<String>, &str) {
    let first = canonical
        .find('.')
        .expect("canonical key has a section dot");
    let last = canonical
        .rfind('.')
        .expect("canonical key has a variable dot");
    let section = &canonical[..first];
    let variable = &canonical[last + 1..];
    if first == last {
        (section, None, variable)
    } else {
        (
            section,
            Some(canonical[first + 1..last].to_string()),
            variable,
        )
    }
}

/// Run `git -C <interpolated-path> <child_args>` as a subprocess, returning its
/// exit code. Re-executes the running `sley` binary so the child is the same
/// implementation, with the repository-local environment sanitized.
fn run_command_on_repo(path: &str, child_args: &[String]) -> Result<i32> {
    let abspath = interpolate_path(path);
    let exe = env::current_exe().map_err(|err| GitError::Io(err.to_string()))?;
    let mut child = ProcessCommand::new(exe);
    for var in LOCAL_REPO_ENV {
        child.env_remove(var);
    }
    child.arg("-C").arg(&abspath);
    child.args(child_args);
    let status = child
        .status()
        .map_err(|err| GitError::Io(err.to_string()))?;
    Ok(status.code().unwrap_or(1))
}

/// git's `interpolate_path` for the cases the config values use: a leading `~/`
/// (or bare `~`) expands to `$HOME`; everything else is taken verbatim.
fn interpolate_path(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}
