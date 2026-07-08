//! Author/committer identity resolution from env and config.

use std::env;
use std::path::PathBuf;

use sley::{GitConfig, GitError, Result};

use crate::argv_bytes_from_os;
use crate::common_git_dir_for_git_dir;
use crate::commands::remote::repo_current_branch_name;
use crate::effective_config_parameters_env;
use crate::global_config_value;
use crate::session;
use crate::sley_config;
use sley_sequencer;


pub(crate) fn commit_identity_from_env(role: &str) -> Result<Vec<u8>> {
    // git's identity precedence for the name/email of an author or committer:
    //   GIT_{role}_NAME/EMAIL env var
    //     -> `-c {author,committer}.name=` / GIT_CONFIG_* command-line overrides
    //       -> effective config {author,committer}.name/email
    //         -> effective config user.name/email
    //           -> sley's built-in default identity
    // Higher-precedence env/`-c`/repo sources are evaluated exactly as before;
    // the global+system config layer is the new fallback below repo config.
    // The effective config is loaded at most once, and only when the env vars do
    // not already supply both fields, so the common env-driven path is unchanged.
    let env_name = env::var_os(format!("GIT_{role}_NAME")).map(argv_bytes_from_os);
    let env_email = env::var_os(format!("GIT_{role}_EMAIL")).map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role(role, "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role(role, "email", &mut config).map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

/// Like [`commit_identity_from_env`] but with the date forced to `date_override`
/// (any form [`canonicalize_commit_date`] accepts), keeping the env/config
/// name+email resolution unchanged. Used by `git am
/// --committer-date-is-author-date`, which keeps the environment committer
/// name/email but substitutes the author date.
pub(crate) fn commit_identity_from_env_with_date(role: &str, date_override: &str) -> Result<Vec<u8>> {
    let env_name = env::var_os(format!("GIT_{role}_NAME")).map(argv_bytes_from_os);
    let env_email = env::var_os(format!("GIT_{role}_EMAIL")).map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role(role, "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role(role, "email", &mut config).map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = canonicalize_commit_date(date_override);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

pub(crate) fn committer_identity_for_reflog() -> Result<Vec<u8>> {
    let env_name = env::var_os("GIT_COMMITTER_NAME").map(argv_bytes_from_os);
    let env_email = env::var_os("GIT_COMMITTER_EMAIL").map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "name", &mut config).map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"Git Rs".to_vec());
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "email", &mut config)
                .map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"sley@example.invalid".to_vec());
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)
}

/// Canonicalise a `GIT_*_DATE`/`--date=` value to git's raw `<seconds> +HHMM`
/// form so the sequencer's identity builder (which only accepts the raw form)
/// stores the same bytes git would.
///
/// git's `commit-tree` / `commit` run author and committer dates through
/// `parse_date`, accepting ISO-8601 (`2005-04-07T22:13:13`), `<date> <time> <tz>`
/// (`2005-01-01 00:00:00 +0000`), RFC-2822, and the raw form. The full date.c
/// port lives in [`commands::approxidate`]; route the value through it and emit
/// the canonical raw form. Values that do not parse are passed through verbatim
/// so the sequencer still reports the original "invalid date" error.
pub(crate) fn canonicalize_commit_date(date: &str) -> String {
    if date.is_empty() {
        return default_commit_date();
    }
    match crate::commands::approxidate::parse_commit_date(date) {
        Some((seconds, tz)) => format!("{seconds} {tz}"),
        None => date.to_string(),
    }
}

pub(crate) fn default_commit_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    format!("{seconds} +0000")
}

/// Lazily-loaded effective config used as the identity fallback. `Skip` means
/// the caller already has both fields from the environment and the config files
/// must not be touched; `Lazy` caches the (optional) loaded config so multiple
/// key lookups share a single load.
pub(crate) enum IdentityConfig {
    Skip,
    Lazy(Option<Option<GitConfig>>),
}

/// Resolve an identity config key (`user.name`/`user.email`) following git's
/// precedence below the environment: `-c`/`GIT_CONFIG_*` command-line overrides
/// first, then the effective config (repository, then global, then system).
pub(crate) fn identity_config_value(key: &str, config: &mut IdentityConfig) -> Option<String> {
    if let Ok(Some(value)) = global_config_value(key) {
        return Some(value);
    }
    let (section, name) = key.split_once('.')?;
    let loaded = match config {
        IdentityConfig::Skip => return None,
        IdentityConfig::Lazy(slot) => slot.get_or_insert_with(identity_effective_config),
    };
    loaded
        .as_ref()
        .and_then(|config| config.get(section, None, name).map(str::to_string))
}

pub(crate) fn identity_config_value_for_role(
    role: &str,
    field: &str,
    config: &mut IdentityConfig,
) -> Option<String> {
    let role_key = match role {
        "AUTHOR" => Some(format!("author.{field}")),
        "COMMITTER" => Some(format!("committer.{field}")),
        _ => None,
    };
    role_key
        .as_deref()
        .and_then(|key| identity_config_value(key, config))
        .or_else(|| identity_config_value(&format!("user.{field}"), config))
}

pub(crate) fn identity_default_value(value: &str, config: &mut IdentityConfig) -> Option<String> {
    if identity_use_config_only(config) {
        None
    } else {
        Some(value.to_string())
    }
}

pub(crate) fn identity_use_config_only(config: &mut IdentityConfig) -> bool {
    identity_config_value("user.useconfigonly", config)
        .as_deref()
        .and_then(sley_config::parse_config_bool)
        .unwrap_or(false)
}

pub(crate) fn identity_use_config_only_error<T>() -> Result<T> {
    eprintln!("fatal: no email was given and auto-detection is disabled");
    Err(GitError::Exit(128))
}

pub(crate) fn validate_commit_identity_name(role: &str, name: &[u8], email: &[u8]) -> Result<()> {
    if name.is_empty() {
        print_identity_unknown_hint(role);
        eprintln!(
            "fatal: empty ident name (for <{}>) not allowed",
            String::from_utf8_lossy(email)
        );
        return Err(GitError::Exit(128));
    }
    if !name.iter().any(|byte| !commit_identity_name_crud(*byte)) {
        eprintln!(
            "fatal: name consists only of disallowed characters: {}",
            String::from_utf8_lossy(name)
        );
        return Err(GitError::Exit(128));
    }
    Ok(())
}

pub(crate) fn commit_identity_name_crud(byte: u8) -> bool {
    matches!(
        byte,
        0..=32 | b',' | b':' | b';' | b'<' | b'>' | b'"' | b'\\' | b'\''
    )
}

pub(crate) fn print_identity_unknown_hint(role: &str) {
    match role {
        "AUTHOR" => eprintln!("Author identity unknown"),
        "COMMITTER" => eprintln!("Committer identity unknown"),
        _ => {}
    }
}

/// Load the effective config (repository + global + system, with includes) for
/// identity fallback, or `None` when there is no repository in scope. Failures
/// degrade to `None` so identity resolution can still fall through to env/`-c`
/// values or the built-in default rather than aborting.
pub(crate) fn identity_effective_config() -> Option<GitConfig> {
    // `cli_git_dir` already honours `--git-dir`/`GIT_DIR` (via
    // `explicit_git_dir`) before walking up from the current directory.
    let git_dir = session::cli_git_dir().ok()?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir).ok()?;
    let context = sley_config::ConfigIncludeContext::new(
        Some(common_git_dir.clone()),
        repo_current_branch_name(&git_dir),
    );
    let mut config = sley_config::load_effective_config(&common_git_dir, &context).ok()?;
    // Layer the command-line `-c`/`--config-env` overrides on top, so reads like
    // `mailmap.blob`/`mailmap.file` see the same values `git config` would (the
    // CLI cannot push `-c` into the process env, so reconstruct it here).
    let parameters_env = effective_config_parameters_env();
    if let Ok(parameters) = sley_config::injected_config_parameters(parameters_env.as_deref()) {
        let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        );
    }
    Some(config)
}

pub(crate) fn commit_signoff_from_env() -> Result<Vec<u8>> {
    // git's `--signoff` uses the committer identity, so resolve it with the same
    // precedence as `commit_identity_from_env("COMMITTER")`.
    let env_name = env::var_os("GIT_COMMITTER_NAME").map(argv_bytes_from_os);
    let env_email = env::var_os("GIT_COMMITTER_EMAIL").map(argv_bytes_from_os);
    let mut config = if env_name.is_none() || env_email.is_none() {
        IdentityConfig::Lazy(None)
    } else {
        IdentityConfig::Skip
    };
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "name", &mut config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", &mut config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "email", &mut config)
                .map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", &mut config).map(String::into_bytes)
        });
    let (Some(name), Some(email)) = (name, email) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name("COMMITTER", &name, &email)?;
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    sley_sequencer::format_commit_identity_bytes(&name, &email, &date)?;
    let mut out = b"Signed-off-by: ".to_vec();
    out.extend_from_slice(&name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&email);
    out.push(b'>');
    Ok(out)
}

pub(crate) fn commit_reflog_message(message: &[u8], amend: bool) -> Vec<u8> {
    commit_reflog_message_with_initial(message, amend, false)
}

pub(crate) fn commit_reflog_message_with_initial(message: &[u8], amend: bool, initial: bool) -> Vec<u8> {
    let subject = String::from_utf8_lossy(message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if amend {
        format!("commit (amend): {subject}").into_bytes()
    } else if initial {
        format!("commit (initial): {subject}").into_bytes()
    } else {
        format!("commit: {subject}").into_bytes()
    }
}

pub(crate) fn default_committer() -> Vec<u8> {
    b"Git Rs <sley@example.invalid> 0 +0000".to_vec()
}
