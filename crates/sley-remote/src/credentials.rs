//! Credential acquisition for authenticated remotes.
//!
//! Derives the credential lookup key for a remote URL, runs `credential.helper`
//! programs to fill in a username/password, and remembers or forgets results.
//! The default [`CredentialHelperProvider`] wraps this as a
//! [`CredentialProvider`](crate::CredentialProvider); embedders targeting public
//! remotes can use [`NoCredentials`](crate::NoCredentials) instead.

use std::io::Write;
use std::process::{Command, Stdio};

use sley_config::GitConfig;
use sley_core::Result;
use sley_transport::{
    encode_git_credential, parse_git_credential, GitCredential, RemoteTransport, RemoteUrl,
};

use crate::CredentialProvider;

/// The `protocol` field of a credential request derived from `remote`.
pub fn http_protocol_name(remote: &RemoteUrl) -> Option<String> {
    match remote.transport {
        RemoteTransport::Https => Some("https".to_string()),
        RemoteTransport::Http => Some("http".to_string()),
        _ => None,
    }
}

/// The `host[:port]` field of a credential request derived from `remote`.
pub fn http_credential_host(remote: &RemoteUrl) -> Option<String> {
    remote.host.clone().map(|host| match remote.port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Credential implied by `user[:password]@` userinfo in the remote URL.
pub fn http_url_credential(remote: &RemoteUrl) -> Option<GitCredential> {
    let username = remote.user.clone()?;
    Some(GitCredential {
        protocol: http_protocol_name(remote),
        host: http_credential_host(remote),
        username: Some(username),
        password: remote.password.clone(),
        ..GitCredential::default()
    })
}

/// The lookup key a credential helper is asked to fill for this remote.
pub fn credential_request_for_url(remote: &RemoteUrl) -> GitCredential {
    GitCredential {
        protocol: http_protocol_name(remote),
        host: http_credential_host(remote),
        username: remote.user.clone(),
        ..GitCredential::default()
    }
}

/// Ordered `credential.helper` values from config. An empty value resets the
/// accumulated list, matching upstream git semantics.
fn credential_helper_specs(config: Option<&GitConfig>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    for section in &config.sections {
        if section.name != "credential" || section.subsection.is_some() {
            continue;
        }
        for entry in &section.entries {
            if !entry.key.eq_ignore_ascii_case("helper") {
                continue;
            }
            match entry.value.as_deref() {
                Some("") | None => specs.clear(),
                Some(value) => specs.push(value.to_string()),
            }
        }
    }
    specs
}

/// Resolve a `credential.helper` spec into a runnable command, appending the
/// operation (`get`/`store`/`erase`). Supports `!shell`, absolute paths, and
/// bare names (mapped to `git-credential-<name>`), each with optional arguments.
fn credential_helper_command(spec: &str, op: &str) -> Option<Command> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    if let Some(shell) = spec.strip_prefix('!') {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{shell} \"$@\""))
            .arg("sh")
            .arg(op);
        return Some(command);
    }
    let mut tokens = spec.split_whitespace();
    let head = tokens.next()?;
    let program = if head.contains('/') {
        head.to_string()
    } else {
        format!("git-credential-{head}")
    };
    let mut command = Command::new(program);
    for arg in tokens {
        command.arg(arg);
    }
    command.arg(op);
    Some(command)
}

/// Run a credential helper, feeding `input` on stdin. Best-effort: a missing or
/// failing helper yields `None` rather than aborting the transfer.
fn run_credential_helper(spec: &str, op: &str, input: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(mut command) = credential_helper_command(spec, op) else {
        return Ok(None);
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(None),
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

/// Fill `request` (username/password) using the configured credential helpers,
/// returning a complete credential or `None` if it could not be completed.
pub fn credential_fill(
    config: Option<&GitConfig>,
    mut request: GitCredential,
) -> Result<Option<GitCredential>> {
    for spec in credential_helper_specs(config) {
        if request.username.is_some() && request.password.is_some() {
            break;
        }
        let input = encode_git_credential(&request)?;
        if let Some(stdout) = run_credential_helper(&spec, "get", &input)? {
            let filled = parse_git_credential(&stdout)?;
            if filled.username.is_some() {
                request.username = filled.username;
            }
            if filled.password.is_some() {
                request.password = filled.password;
            }
        }
    }
    if request.username.is_some() && request.password.is_some() {
        Ok(Some(request))
    } else {
        Ok(None)
    }
}

/// Tell the configured helpers to store (`approve = true`) or erase a credential.
pub fn credential_store(config: Option<&GitConfig>, credential: &GitCredential, approve: bool) {
    let Ok(input) = encode_git_credential(credential) else {
        return;
    };
    let op = if approve { "store" } else { "erase" };
    for spec in credential_helper_specs(config) {
        let _ = run_credential_helper(&spec, op, &input);
    }
}

/// The default [`CredentialProvider`]: fills and stores credentials via the
/// repository's configured `credential.helper` programs.
pub struct CredentialHelperProvider<'a> {
    config: Option<&'a GitConfig>,
}

impl<'a> CredentialHelperProvider<'a> {
    /// Create a provider backed by `config`'s `credential.helper` settings.
    pub fn new(config: Option<&'a GitConfig>) -> Self {
        Self { config }
    }
}

impl CredentialProvider for CredentialHelperProvider<'_> {
    fn fill(&mut self, request: GitCredential) -> Result<Option<GitCredential>> {
        credential_fill(self.config, request)
    }

    fn approve(&mut self, credential: &GitCredential) -> Result<()> {
        credential_store(self.config, credential, true);
        Ok(())
    }

    fn reject(&mut self, credential: &GitCredential) -> Result<()> {
        credential_store(self.config, credential, false);
        Ok(())
    }
}
