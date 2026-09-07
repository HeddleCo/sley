use std::fmt;

use sley_config::GitConfig;
use sley_core::GitError;
use sley_transport::{RemoteTransport, RemoteUrl};

pub use sley_transport::TransportPolicy;

/// Namespace and transport policy for one remote operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePolicy {
    pub namespace: sley_core::Namespace,
    pub transport: TransportPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportPolicyError {
    NotAllowed { scheme: String },
    UnknownConfigValue { key: String, value: String },
}

impl fmt::Display for TransportPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAllowed { scheme } => write!(f, "transport '{scheme}' not allowed"),
            Self::UnknownConfigValue { key, value } => {
                write!(f, "unknown value for config '{key}': {value}")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolAllow {
    Never,
    UserOnly,
    Always,
}

/// Classify the policy protocol name for a raw transport URL.
///
/// This mirrors the names `transport_check_allowed()` receives in upstream Git:
/// local paths and `file://` are `file`; scp-like and SSH URL forms are `ssh`;
/// `git+ssh://` and `ssh+git://` are deprecated aliases for `ssh`; `<name>::...`
/// and unknown URL schemes use their helper name.
pub fn transport_scheme_for_url(url: &str) -> String {
    if let Some(helper_end) = helper_scheme_end(url) {
        return url[..helper_end].to_ascii_lowercase();
    }
    if let Some(scheme_end) = url.find("://") {
        let scheme = url[..scheme_end].to_ascii_lowercase();
        return match scheme.as_str() {
            "git+ssh" | "ssh+git" => "ssh".to_string(),
            _ => scheme,
        };
    }
    if scp_like_separator(url).is_some() {
        "ssh".to_string()
    } else {
        "file".to_string()
    }
}

pub fn transport_scheme_for_remote(remote: &RemoteUrl) -> &'static str {
    match remote.transport {
        RemoteTransport::Local | RemoteTransport::File => "file",
        RemoteTransport::Ext => "ext",
        RemoteTransport::Ssh => "ssh",
        RemoteTransport::Git => "git",
        RemoteTransport::Http => "http",
        RemoteTransport::Https => "https",
    }
}

pub fn check_transport_allowed(
    scheme: &str,
    config: Option<&GitConfig>,
    policy: &TransportPolicy,
) -> std::result::Result<(), TransportPolicyError> {
    if is_transport_allowed(scheme, config, policy)? {
        Ok(())
    } else {
        Err(TransportPolicyError::NotAllowed {
            scheme: scheme.to_string(),
        })
    }
}

pub(crate) fn transport_policy_git_error(err: TransportPolicyError) -> GitError {
    GitError::InvalidFormat(format!("fatal: {err}"))
}

pub fn is_transport_allowed(
    scheme: &str,
    config: Option<&GitConfig>,
    policy: &TransportPolicy,
) -> std::result::Result<bool, TransportPolicyError> {
    if let Some(allow) = &policy.allow_protocols {
        return Ok(allow.iter().any(|entry| entry == scheme));
    }
    Ok(match protocol_config(scheme, config)? {
        ProtocolAllow::Always => true,
        ProtocolAllow::Never => false,
        ProtocolAllow::UserOnly => policy.from_user,
    })
}

fn protocol_config(
    scheme: &str,
    config: Option<&GitConfig>,
) -> std::result::Result<ProtocolAllow, TransportPolicyError> {
    if let Some(config) = config {
        let key = format!("protocol.{scheme}.allow");
        if let Some(value) = config.get("protocol", Some(scheme), "allow") {
            return parse_protocol_config(&key, value);
        }
        if let Some(value) = config.get("protocol", None, "allow") {
            return parse_protocol_config("protocol.allow", value);
        }
    }
    Ok(match scheme {
        "http" | "https" | "git" | "ssh" => ProtocolAllow::Always,
        "ext" => ProtocolAllow::Never,
        _ => ProtocolAllow::UserOnly,
    })
}

fn parse_protocol_config(
    key: &str,
    value: &str,
) -> std::result::Result<ProtocolAllow, TransportPolicyError> {
    if value.eq_ignore_ascii_case("always") {
        Ok(ProtocolAllow::Always)
    } else if value.eq_ignore_ascii_case("never") {
        Ok(ProtocolAllow::Never)
    } else if value.eq_ignore_ascii_case("user") {
        Ok(ProtocolAllow::UserOnly)
    } else {
        Err(TransportPolicyError::UnknownConfigValue {
            key: key.to_string(),
            value: value.to_string(),
        })
    }
}

fn helper_scheme_end(url: &str) -> Option<usize> {
    let mut chars = url.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    for (idx, ch) in chars {
        if ch == ':' {
            return url[idx..].starts_with("::").then_some(idx);
        }
        if !is_url_scheme_char(ch) {
            return None;
        }
    }
    None
}

fn is_url_scheme_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')
}

fn scp_like_separator(value: &str) -> Option<usize> {
    let colon = if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']')?;
        let colon = close + 2;
        if value.as_bytes().get(colon) == Some(&b':') {
            colon
        } else {
            return None;
        }
    } else {
        value.find(':')?
    };
    if value[..colon].contains('/') {
        return None;
    }
    if colon == 1
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && (value.as_bytes().get(2) == Some(&b'/') || cfg!(windows))
    {
        return None;
    }
    Some(colon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_config::{ConfigEntry, ConfigSection};

    fn config(section: ConfigSection) -> GitConfig {
        GitConfig {
            sections: vec![section],
            ..GitConfig::default()
        }
    }

    #[test]
    fn classifies_builtin_and_helper_url_forms() {
        assert_eq!(transport_scheme_for_url("/repo.git"), "file");
        assert_eq!(transport_scheme_for_url("file:///repo.git"), "file");
        assert_eq!(transport_scheme_for_url("git://host/repo.git"), "git");
        assert_eq!(transport_scheme_for_url("ssh://host/repo.git"), "ssh");
        assert_eq!(transport_scheme_for_url("git+ssh://host/repo.git"), "ssh");
        assert_eq!(transport_scheme_for_url("user@host:repo.git"), "ssh");
        assert_eq!(
            transport_scheme_for_url("ext::fake-remote %S repo.git"),
            "ext"
        );
        assert_eq!(transport_scheme_for_url("foo://host/repo.git"), "foo");
    }

    #[test]
    fn user_policy_honors_explicit_origin() {
        let cfg = config(ConfigSection::new(
            "protocol",
            Some("file".into()),
            vec![ConfigEntry::new("allow", Some("user".into()))],
        ));
        assert!(
            is_transport_allowed("file", Some(&cfg), &TransportPolicy::default())
                .expect("file transport should evaluate user-allowed policy")
        );
        assert!(
            !is_transport_allowed(
                "file",
                Some(&cfg),
                &TransportPolicy {
                    from_user: false,
                    ..TransportPolicy::default()
                }
            )
            .expect("file transport should evaluate user-denied policy")
        );
    }
}
