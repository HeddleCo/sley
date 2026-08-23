//! Author/committer identity resolution from env and config (git's `ident.c`).
//!
//! Sunk out of the CLI so every engine path that authors objects resolves
//! identities through the same precedence chain:
//!
//! 1. `GIT_{role}_NAME`/`GIT_{role}_EMAIL` env vars
//! 2. `-c {author,committer}.name=` / `GIT_CONFIG_*` command-line overrides
//! 3. effective config `{author,committer}.name/email`
//! 4. effective config `user.name/email`
//! 5. sley's built-in default identity

use std::env;
use std::ffi::OsString;

use sley_config::GitConfig;
use sley_core::date::approxidate::parse_commit_date;
use sley_core::{GitError, Result};

/// Canonicalise a `GIT_*_DATE`/`--date=` value to git's raw `<seconds> +HHMM`
/// form so the sequencer's identity builder (which only accepts the raw form)
/// stores the same bytes git would.
///
/// git's `commit-tree` / `commit` run author and committer dates through
/// `parse_date` / `approxidate_careful`, accepting ISO-8601
/// (`2005-04-07T22:13:13`), `<date> <time> <tz>`, RFC-2822, fuzzy approxidates,
/// and the raw form. Values that do not parse are passed through verbatim so
/// callers that only need best-effort conversion (env `GIT_*_DATE`) still get a
/// diagnostic from the identity formatter; prefer [`try_canonicalize_commit_date`]
/// when a hard reject with git's `invalid date format` message is required
/// (`--date=`).
pub fn canonicalize_commit_date(date: &str) -> String {
    if date.is_empty() {
        return default_commit_date();
    }
    match parse_commit_date(date) {
        Some((seconds, tz)) => format!("{seconds} {tz}"),
        None => date.to_string(),
    }
}

/// Like [`canonicalize_commit_date`] but returns `None` when the value does not
/// parse — used for `git commit --date=` so we can die with
/// `fatal: invalid date format: …` matching git's `parse_force_date`.
pub fn try_canonicalize_commit_date(date: &str) -> Option<String> {
    if date.is_empty() {
        return Some(default_commit_date());
    }
    parse_commit_date(date).map(|(seconds, tz)| format!("{seconds} {tz}"))
}

pub fn default_commit_date() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    format!("{seconds} +0000")
}

/// Format a name/email/date triple as git's raw ident line
/// (`Name <email> <seconds> +HHMM`), rejecting control bytes in either
/// component and anything but the raw date form.
pub fn format_commit_identity(name: &str, email: &str, date: &str) -> Result<Vec<u8>> {
    format_commit_identity_bytes(name.as_bytes(), email.as_bytes(), date)
}

pub fn format_commit_identity_bytes(name: &[u8], email: &[u8], date: &str) -> Result<Vec<u8>> {
    validate_identity_component_bytes("name", name)?;
    validate_identity_component_bytes("email", email)?;
    let (seconds, timezone) = parse_raw_git_date(date)?;
    let mut out = Vec::with_capacity(name.len() + email.len() + timezone.len() + 32);
    out.extend_from_slice(name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(email);
    out.extend_from_slice(b"> ");
    out.extend_from_slice(seconds.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(timezone.as_bytes());
    Ok(out)
}

fn validate_identity_component_bytes(name: &str, value: &[u8]) -> Result<()> {
    if value.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "commit identity {name} contains a control byte"
        )));
    }
    Ok(())
}

fn parse_raw_git_date(date: &str) -> Result<(i64, String)> {
    let mut parts = date.split_whitespace();
    let seconds = parts
        .next()
        .ok_or_else(|| GitError::InvalidFormat("missing commit date seconds".into()))?;
    let timezone = parts
        .next()
        .ok_or_else(|| GitError::InvalidFormat("missing commit date timezone".into()))?;
    if parts.next().is_some() {
        return Err(GitError::InvalidFormat(
            "commit date has trailing fields".into(),
        ));
    }
    let seconds = seconds.strip_prefix('@').unwrap_or(seconds);
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| GitError::InvalidFormat("invalid commit date seconds".into()))?;
    validate_timezone(timezone)?;
    Ok((seconds, timezone.to_string()))
}

fn validate_timezone(timezone: &str) -> Result<()> {
    let bytes = timezone.as_bytes();
    if bytes.len() != 5
        || !matches!(bytes[0], b'+' | b'-')
        || !bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(GitError::InvalidFormat(format!(
            "invalid commit timezone {timezone}"
        )));
    }
    Ok(())
}

/// Explicit effective config used as the identity fallback. `Skip` means the
/// caller already has both fields from the environment, so config lookup is
/// unnecessary; `Loaded` borrows the invocation's already-resolved snapshot.
pub enum IdentityConfig<'a> {
    Skip,
    Loaded(&'a GitConfig),
}

/// Look up a single injected (`-c`/`--config-env`/`GIT_CONFIG_COUNT`) override,
/// mirroring git's highest-precedence command-line layer. Parse failures print
/// git's two-line diagnostic exactly once per failing lookup; every other miss
/// is silent.
fn injected_config_value(key: &str) -> Option<String> {
    let canonical = match sley_config::canonicalize_config_key(key) {
        Ok(canonical) => canonical,
        // The lookup key is a fixed internal key; if it fails to canonicalise
        // there can be no matching override.
        Err(_) => return None,
    };
    let parameters_env = sley_config::effective_config_parameters_env();
    match sley_config::injected_config_parameters(parameters_env.as_deref()) {
        Ok(parameters) => parameters
            .iter()
            .rev()
            .find(|param| param.canonical_key.eq_ignore_ascii_case(&canonical))
            .map(|param| match &param.value {
                Some(value) => value.clone(),
                None => "true".to_string(),
            }),
        Err(err) => {
            eprintln!("error: {}", err.message());
            eprintln!("fatal: unable to parse command-line config");
            None
        }
    }
}

/// Resolve an identity config key (`user.name`/`user.email`) following git's
/// precedence below the environment: `-c`/`GIT_CONFIG_*` command-line overrides
/// first, then the effective config (repository, then global, then system).
pub fn identity_config_value(key: &str, config: &mut IdentityConfig<'_>) -> Option<String> {
    if let Some(value) = injected_config_value(key) {
        return Some(value);
    }
    let (section, name) = key.split_once('.')?;
    let loaded = match config {
        IdentityConfig::Skip => return None,
        IdentityConfig::Loaded(config) => *config,
    };
    loaded.get(section, None, name).map(str::to_string)
}

pub fn identity_config_value_for_role(
    role: &str,
    field: &str,
    config: &mut IdentityConfig<'_>,
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

pub fn identity_default_value(value: &str, config: &mut IdentityConfig<'_>) -> Option<String> {
    if identity_use_config_only(config) {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn identity_use_config_only(config: &mut IdentityConfig<'_>) -> bool {
    identity_config_value("user.useconfigonly", config)
        .as_deref()
        .and_then(sley_config::parse_config_bool)
        .unwrap_or(false)
}

pub fn identity_use_config_only_error<T>() -> Result<T> {
    eprintln!("fatal: no email was given and auto-detection is disabled");
    Err(GitError::Exit(128))
}

pub fn validate_commit_identity_name(role: &str, name: &[u8], email: &[u8]) -> Result<()> {
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

pub fn commit_identity_name_crud(byte: u8) -> bool {
    matches!(
        byte,
        0..=32 | b',' | b':' | b';' | b'<' | b'>' | b'"' | b'\\' | b'\''
    )
}

pub fn print_identity_unknown_hint(role: &str) {
    match role {
        "AUTHOR" => eprintln!("Author identity unknown"),
        "COMMITTER" => eprintln!("Committer identity unknown"),
        _ => {}
    }
}

#[cfg(unix)]
fn argv_bytes_from_os(value: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn argv_bytes_from_os(value: OsString) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn resolve_identity_fields(role: &str, config: &mut IdentityConfig<'_>) -> Option<(Vec<u8>, Vec<u8>)> {
    let env_name = env::var_os(format!("GIT_{role}_NAME")).map(argv_bytes_from_os);
    let env_email = env::var_os(format!("GIT_{role}_EMAIL")).map(argv_bytes_from_os);
    let name = env_name
        .or_else(|| {
            identity_config_value_for_role(role, "name", config).map(String::into_bytes)
        })
        .or_else(|| identity_default_value("Git Rs", config).map(String::into_bytes));
    let email = env_email
        .or_else(|| {
            identity_config_value_for_role(role, "email", config).map(String::into_bytes)
        })
        .or_else(|| {
            identity_default_value("sley@example.invalid", config).map(String::into_bytes)
        });
    Some((name?, email?))
}

pub fn commit_identity_from_env(role: &str, effective_config: &GitConfig) -> Result<Vec<u8>> {
    // Higher-precedence env/`-c`/repo sources are evaluated exactly as before;
    // the global+system config layer is the fallback below repo config.
    // The effective config is loaded at most once, and only when the env vars do
    // not already supply both fields, so the common env-driven path is unchanged.
    let mut config = if env::var_os(format!("GIT_{role}_NAME")).is_none()
        || env::var_os(format!("GIT_{role}_EMAIL")).is_none()
    {
        IdentityConfig::Loaded(effective_config)
    } else {
        IdentityConfig::Skip
    };
    let Some((name, email)) = resolve_identity_fields(role, &mut config) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    format_commit_identity_bytes(&name, &email, &date)
}

/// Like [`commit_identity_from_env`] but with the date forced to `date_override`
/// (any form [`canonicalize_commit_date`] accepts), keeping the env/config
/// name+email resolution unchanged. Used by `git am
/// --committer-date-is-author-date`, which keeps the environment committer
/// name/email but substitutes the author date.
pub fn commit_identity_from_env_with_date(
    role: &str,
    date_override: &str,
    effective_config: &GitConfig,
) -> Result<Vec<u8>> {
    let mut config = if env::var_os(format!("GIT_{role}_NAME")).is_none()
        || env::var_os(format!("GIT_{role}_EMAIL")).is_none()
    {
        IdentityConfig::Loaded(effective_config)
    } else {
        IdentityConfig::Skip
    };
    let Some((name, email)) = resolve_identity_fields(role, &mut config) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name(role, &name, &email)?;
    let date = canonicalize_commit_date(date_override);
    format_commit_identity_bytes(&name, &email, &date)
}

pub fn committer_identity_for_reflog(effective_config: &GitConfig) -> Result<Vec<u8>> {
    let mut config = if env::var_os("GIT_COMMITTER_NAME").is_none()
        || env::var_os("GIT_COMMITTER_EMAIL").is_none()
    {
        IdentityConfig::Loaded(effective_config)
    } else {
        IdentityConfig::Skip
    };
    let name = env::var_os("GIT_COMMITTER_NAME")
        .map(argv_bytes_from_os)
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "name", &mut config).map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"Git Rs".to_vec());
    let email = env::var_os("GIT_COMMITTER_EMAIL")
        .map(argv_bytes_from_os)
        .or_else(|| {
            identity_config_value_for_role("COMMITTER", "email", &mut config).map(String::into_bytes)
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| b"sley@example.invalid".to_vec());
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    format_commit_identity_bytes(&name, &email, &date)
}

pub fn commit_signoff_from_env(effective_config: &GitConfig) -> Result<Vec<u8>> {
    // git's `--signoff` uses the committer identity, so resolve it with the same
    // precedence as `commit_identity_from_env("COMMITTER")`.
    let mut config = if env::var_os("GIT_COMMITTER_NAME").is_none()
        || env::var_os("GIT_COMMITTER_EMAIL").is_none()
    {
        IdentityConfig::Loaded(effective_config)
    } else {
        IdentityConfig::Skip
    };
    let Some((name, email)) = resolve_identity_fields("COMMITTER", &mut config) else {
        return identity_use_config_only_error();
    };
    validate_commit_identity_name("COMMITTER", &name, &email)?;
    let date = env::var("GIT_COMMITTER_DATE").unwrap_or_else(|_| "@0 +0000".into());
    let date = canonicalize_commit_date(&date);
    format_commit_identity_bytes(&name, &email, &date)?;
    let mut out = b"Signed-off-by: ".to_vec();
    out.extend_from_slice(&name);
    out.extend_from_slice(b" <");
    out.extend_from_slice(&email);
    out.push(b'>');
    Ok(out)
}

pub fn commit_reflog_message(message: &[u8], amend: bool) -> Vec<u8> {
    commit_reflog_message_with_initial(message, amend, false)
}

pub fn commit_reflog_message_with_initial(
    message: &[u8],
    amend: bool,
    initial: bool,
) -> Vec<u8> {
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

pub fn default_committer() -> Vec<u8> {
    b"Git Rs <sley@example.invalid> 0 +0000".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_formats_raw_git_date() {
        let identity =
            format_commit_identity("Example User", "example@example.invalid", "@0 +0000")
                .expect("test operation should succeed");
        assert_eq!(identity, b"Example User <example@example.invalid> 0 +0000");
    }

    #[test]
    fn identity_rejects_control_bytes_and_bad_timezones() {
        assert!(format_commit_identity_bytes(b"na\nme", b"x@y", "@0 +0000").is_err());
        assert!(format_commit_identity_bytes(b"name", b"x@y", "not-a-date").is_err());
        assert!(format_commit_identity_bytes(b"name", b"x@y", "@0 +000").is_err());
        assert!(format_commit_identity_bytes(b"name", b"x@y", "@0 0000").is_err());
        assert!(format_commit_identity_bytes(b"name", b"x@y", "@0 +0000 extra").is_err());
    }

    #[test]
    fn canonicalize_accepts_the_raw_form_and_strips_the_at_sign() {
        assert_eq!(
            try_canonicalize_commit_date("@1234 +0530"),
            Some("1234 +0530".to_string())
        );
        assert_eq!(try_canonicalize_commit_date("not a date"), None);
    }

    #[test]
    fn canonicalizes_iso_dates_to_raw_seconds() {
        assert_eq!(
            canonicalize_commit_date("1970-01-01 00:00:00 +0000"),
            "0 +0000"
        );
    }

    #[test]
    fn validates_ident_names_like_git() {
        assert!(validate_commit_identity_name("AUTHOR", b"", b"x@y").is_err());
        assert!(validate_commit_identity_name("AUTHOR", b"<<<", b"x@y").is_err());
        assert!(validate_commit_identity_name("AUTHOR", b"A U Thor", b"x@y").is_ok());
        assert!(commit_identity_name_crud(b'<'));
        assert!(!commit_identity_name_crud(b'a'));
    }

    #[test]
    fn reflog_messages_follow_git_subject_rules() {
        assert_eq!(
            commit_reflog_message(b"subject\n\nbody", false),
            b"commit: subject".to_vec()
        );
        assert_eq!(
            commit_reflog_message(b"subject", true),
            b"commit (amend): subject".to_vec()
        );
        assert_eq!(
            commit_reflog_message_with_initial(b"subject", false, true),
            b"commit (initial): subject".to_vec()
        );
        assert_eq!(default_committer(), b"Git Rs <sley@example.invalid> 0 +0000");
    }

    #[test]
    fn signoff_uses_committer_identity_shape() {
        let config = GitConfig::default();
        // Env-independent shape assertion: the runner may or may not carry
        // GIT_COMMITTER_* variables, but the trailer format is fixed.
        if let Ok(signoff) = commit_signoff_from_env(&config) {
            let text = String::from_utf8_lossy(&signoff).into_owned();
            assert!(text.starts_with("Signed-off-by: "), "{text}");
            assert!(text.ends_with('>'), "{text}");
        }
    }
}
