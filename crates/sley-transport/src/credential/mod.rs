//! Git credential protocol engine (`credential.c` parity).

#[cfg(unix)]
mod cache;
#[cfg(unix)]
mod cache_daemon;
mod prompt;
mod store;
#[cfg(unix)]
mod unix_socket;
mod url;

use std::io::{self, BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_config::{ConfigStack, GitConfig};
use sley_core::{GitError, Result};

#[cfg(unix)]
pub use cache::cmd_credential_cache;
#[cfg(unix)]
pub use cache_daemon::cmd_credential_cache_daemon;
pub use store::cmd_credential_store;

pub const TIME_MAX: i64 = i64::MAX;
const MAX_GIT_CREDENTIAL_RESPONSE_BYTES: usize = 64 * 1024;

#[cfg(not(unix))]
pub fn cmd_credential_cache(args: &[String]) -> Result<()> {
    let _ = args;
    Err(GitError::Command(credential_cache_unsupported_message(
        "credential-cache",
    )))
}

#[cfg(not(unix))]
pub fn cmd_credential_cache_daemon(args: &[String]) -> Result<()> {
    let _ = args;
    Err(GitError::Command(credential_cache_unsupported_message(
        "credential-cache--daemon",
    )))
}

#[cfg(all(not(unix), windows))]
fn credential_cache_unsupported_message(command: &str) -> String {
    format!("{command} is unsupported on Windows")
}

#[cfg(all(not(unix), not(windows)))]
fn credential_cache_unsupported_message(command: &str) -> String {
    format!("{command} unavailable; no unix socket support")
}

#[cfg(all(test, not(unix)))]
mod non_unix_cache_tests {
    use super::*;

    #[test]
    fn credential_cache_reports_platform_unsupported() {
        #[cfg(windows)]
        assert_eq!(
            cmd_credential_cache(&[]).unwrap_err(),
            GitError::Command("credential-cache is unsupported on Windows".into())
        );
        #[cfg(not(windows))]
        assert_eq!(
            cmd_credential_cache(&[]).unwrap_err(),
            GitError::Command("credential-cache unavailable; no unix socket support".into())
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialOpType {
    Initial,
    Helper,
    Response,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CredentialCapability {
    pub request_initial: bool,
    pub request_helper: bool,
    pub response: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCredential {
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_expiry_utc: i64,
    pub oauth_refresh_token: Option<String>,
    pub authtype: Option<String>,
    pub credential: Option<String>,
    pub ephemeral: bool,
    pub url: Option<String>,
    pub wwwauth: Vec<String>,
    pub state: Vec<String>,
    pub state_to_send: Vec<String>,
    pub quit: bool,
    pub multistage: bool,
    pub approved: bool,
    pub configured: bool,
    pub username_from_proto: bool,
    pub use_http_path: bool,
    pub sanitize_prompt: bool,
    pub protect_protocol: bool,
    pub helpers: Vec<String>,
    pub capa_authtype: CredentialCapability,
    pub capa_state: CredentialCapability,
    pub extra: Vec<(String, String)>,
}

impl Default for GitCredential {
    fn default() -> Self {
        Self {
            protocol: None,
            host: None,
            path: None,
            username: None,
            password: None,
            password_expiry_utc: TIME_MAX,
            oauth_refresh_token: None,
            authtype: None,
            credential: None,
            ephemeral: false,
            url: None,
            wwwauth: Vec::new(),
            state: Vec::new(),
            state_to_send: Vec::new(),
            quit: false,
            multistage: false,
            approved: false,
            configured: false,
            username_from_proto: false,
            use_http_path: false,
            // Git enables both defenses in CREDENTIAL_INIT. Configuration may
            // explicitly opt out, but an unconfigured credential must never
            // put control bytes into a prompt or a credential-protocol line.
            sanitize_prompt: true,
            protect_protocol: true,
            helpers: Vec::new(),
            capa_authtype: CredentialCapability::default(),
            capa_state: CredentialCapability::default(),
            extra: Vec::new(),
        }
    }
}

impl GitCredential {
    pub fn is_full(&self) -> bool {
        (self.username.is_some() && self.password.is_some()) || self.credential.is_some()
    }
}

pub fn credential_set_all_capabilities(credential: &mut GitCredential, op_type: CredentialOpType) {
    set_capability(&mut credential.capa_authtype, op_type);
    set_capability(&mut credential.capa_state, op_type);
}

fn set_capability(capa: &mut CredentialCapability, op_type: CredentialOpType) {
    match op_type {
        CredentialOpType::Initial => capa.request_initial = true,
        CredentialOpType::Helper => capa.request_helper = true,
        CredentialOpType::Response => capa.response = true,
    }
}

pub fn credential_has_capability(capa: &CredentialCapability, op_type: CredentialOpType) -> bool {
    match op_type {
        CredentialOpType::Helper => capa.request_initial,
        CredentialOpType::Response => capa.request_initial && capa.request_helper,
        CredentialOpType::Initial => false,
    }
}

pub fn credential_announce_capabilities(
    credential: &GitCredential,
    writer: &mut impl Write,
) -> Result<()> {
    writeln!(writer, "version 0").map_err(|e| GitError::Io(e.to_string()))?;
    if credential.capa_authtype.request_initial {
        writeln!(writer, "capability authtype").map_err(|e| GitError::Io(e.to_string()))?;
    }
    if credential.capa_state.request_initial {
        writeln!(writer, "capability state").map_err(|e| GitError::Io(e.to_string()))?;
    }
    Ok(())
}

pub fn credential_next_state(credential: &mut GitCredential) {
    credential.state_to_send = std::mem::take(&mut credential.state);
}

pub fn credential_clear_secrets(credential: &mut GitCredential) {
    credential.password = None;
    credential.credential = None;
}

pub fn credential_read(
    credential: &mut GitCredential,
    reader: &mut impl BufRead,
    op_type: CredentialOpType,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
        if line == "\n" || line == "\r\n" {
            break;
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        apply_credential_line(credential, &line, op_type)?;
    }
    Ok(())
}

pub(crate) fn apply_credential_line(
    credential: &mut GitCredential,
    line: &str,
    op_type: CredentialOpType,
) -> Result<()> {
    let Some((key, value)) = line.split_once('=') else {
        eprintln!("warning: invalid credential line: {line}");
        return Err(GitError::InvalidFormat(
            "credential line is missing = delimiter".into(),
        ));
    };
    match key {
        "username" => {
            credential.username = Some(value.to_string());
            credential.username_from_proto = true;
        }
        "password" => credential.password = Some(value.to_string()),
        "credential" => credential.credential = Some(value.to_string()),
        "protocol" => credential.protocol = Some(value.to_string()),
        "host" => credential.host = Some(value.to_string()),
        "path" => credential.path = Some(value.to_string()),
        "ephemeral" => credential.ephemeral = parse_bool(value),
        "wwwauth[]" => credential.wwwauth.push(value.to_string()),
        "state[]" => credential.state.push(value.to_string()),
        "capability[]" => match value {
            "authtype" => set_capability(&mut credential.capa_authtype, op_type),
            "state" => set_capability(&mut credential.capa_state, op_type),
            _ => {}
        },
        "continue" => credential.multistage = parse_bool(value),
        "password_expiry_utc" => credential.password_expiry_utc = url::parse_timestamp(value),
        "oauth_refresh_token" => credential.oauth_refresh_token = Some(value.to_string()),
        "authtype" => credential.authtype = Some(value.to_string()),
        "url" => url::credential_from_url(credential, value)?,
        "quit" => credential.quit = parse_bool(value),
        _ => credential.extra.push((key.to_string(), value.to_string())),
    }
    Ok(())
}

fn parse_bool(value: &str) -> bool {
    sley_config::parse_config_bool(value).unwrap_or(!value.is_empty())
}

pub fn credential_write(
    credential: &GitCredential,
    writer: &mut impl Write,
    op_type: CredentialOpType,
) -> Result<()> {
    if credential_has_capability(&credential.capa_authtype, op_type) {
        write_item(credential, writer, "capability[]", Some("authtype"), false)?;
    }
    if credential_has_capability(&credential.capa_state, op_type) {
        write_item(credential, writer, "capability[]", Some("state"), false)?;
    }
    if credential_has_capability(&credential.capa_authtype, op_type) {
        write_item(
            credential,
            writer,
            "authtype",
            credential.authtype.as_deref(),
            false,
        )?;
        write_item(
            credential,
            writer,
            "credential",
            credential.credential.as_deref(),
            false,
        )?;
        if credential.ephemeral {
            write_item(credential, writer, "ephemeral", Some("1"), false)?;
        }
    }
    write_item(
        credential,
        writer,
        "protocol",
        credential.protocol.as_deref(),
        true,
    )?;
    write_item(credential, writer, "host", credential.host.as_deref(), true)?;
    write_item(
        credential,
        writer,
        "path",
        credential.path.as_deref(),
        false,
    )?;
    write_item(
        credential,
        writer,
        "username",
        credential.username.as_deref(),
        false,
    )?;
    write_item(
        credential,
        writer,
        "password",
        credential.password.as_deref(),
        false,
    )?;
    write_item(
        credential,
        writer,
        "oauth_refresh_token",
        credential.oauth_refresh_token.as_deref(),
        false,
    )?;
    if credential.password_expiry_utc != TIME_MAX {
        write_item(
            credential,
            writer,
            "password_expiry_utc",
            Some(&credential.password_expiry_utc.to_string()),
            false,
        )?;
    }
    for value in &credential.wwwauth {
        write_item(credential, writer, "wwwauth[]", Some(value.as_str()), false)?;
    }
    if credential_has_capability(&credential.capa_state, op_type) {
        if credential.multistage {
            write_item(credential, writer, "continue", Some("1"), false)?;
        }
        for value in &credential.state_to_send {
            write_item(credential, writer, "state[]", Some(value.as_str()), false)?;
        }
    }
    Ok(())
}

fn write_item(
    credential: &GitCredential,
    writer: &mut impl Write,
    key: &str,
    value: Option<&str>,
    required: bool,
) -> Result<()> {
    let Some(value) = value else {
        if required {
            return Err(GitError::InvalidFormat(format!(
                "credential value for {key} is missing"
            )));
        }
        return Ok(());
    };
    if value.contains('\n') {
        return Err(GitError::InvalidFormat(format!(
            "credential value for {key} contains newline"
        )));
    }
    if credential.protect_protocol && value.contains('\r') {
        return Err(GitError::InvalidFormat(format!(
            "fatal: credential value for {key} contains carriage return\n\
             If this is intended, set `credential.protectProtocol=false`"
        )));
    }
    writeln!(writer, "{key}={value}").map_err(|e| GitError::Io(e.to_string()))?;
    Ok(())
}

pub fn credential_helper_specs(config: Option<&GitConfig>) -> Vec<String> {
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

pub fn credential_helper_command(spec: &str, op: &str) -> Option<Command> {
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
    if head.starts_with('/') {
        let mut cmd = head.to_string();
        for arg in tokens {
            cmd.push(' ');
            cmd.push_str(arg);
        }
        cmd.push(' ');
        cmd.push_str(op);
        let mut command = Command::new("sh");
        command.arg("-c").arg(cmd);
        return Some(command);
    }
    let exe = std::env::current_exe().ok()?;
    let mut shell_cmd = format!("{} credential-{}", exe.display(), spec);
    shell_cmd.push(' ');
    shell_cmd.push_str(op);
    let mut command = Command::new("sh");
    command.arg("-c").arg(shell_cmd);
    Some(command)
}

fn credential_do(
    credential: &mut GitCredential,
    helper: &str,
    operation: &str,
    want_output: bool,
) -> Result<()> {
    let Some(mut command) = credential_helper_command(helper, operation) else {
        return Err(GitError::Command("empty credential helper".into()));
    };
    command.stdin(Stdio::piped());
    command.stdout(if want_output {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command.spawn().map_err(|e| GitError::Io(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let op_type = if want_output {
            CredentialOpType::Helper
        } else {
            CredentialOpType::Response
        };
        credential_write(credential, &mut stdin, op_type)?;
    }
    if want_output {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitError::Io("credential helper stdout was not piped".into()))?;
        let mut input = Vec::new();
        stdout
            .take((MAX_GIT_CREDENTIAL_RESPONSE_BYTES as u64).saturating_add(1))
            .read_to_end(&mut input)
            .map_err(|e| GitError::Io(e.to_string()))?;
        if input.len() > MAX_GIT_CREDENTIAL_RESPONSE_BYTES {
            return Err(GitError::InvalidFormat(format!(
                "credential helper response exceeds maximum size of {} bytes (64 KiB)",
                MAX_GIT_CREDENTIAL_RESPONSE_BYTES
            )));
        }
        let status = child.wait().map_err(|e| GitError::Io(e.to_string()))?;
        if !status.success() {
            return Err(GitError::Command(format!(
                "credential helper '{helper}' failed"
            )));
        }
        let mut reader = io::Cursor::new(input);
        credential_read(credential, &mut reader, CredentialOpType::Helper)?;
    } else {
        let status = child.wait().map_err(|e| GitError::Io(e.to_string()))?;
        if !status.success() {
            return Err(GitError::Command(format!(
                "credential helper '{helper}' failed"
            )));
        }
    }
    Ok(())
}

pub fn credential_apply_config(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
) -> Result<()> {
    url::credential_apply_config(config, stack, credential)
}

pub fn credential_fill(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
    all_capabilities: bool,
) -> Result<()> {
    if credential.is_full() {
        return Ok(());
    }
    credential_next_state(credential);
    credential.multistage = false;
    credential_apply_config(config, stack, credential)?;
    if all_capabilities {
        credential_set_all_capabilities(credential, CredentialOpType::Initial);
    }
    let helpers = credential.helpers.clone();
    for helper in helpers {
        credential_do(credential, &helper, "get", true)?;
        if credential.password_expiry_utc < now() {
            credential_clear_secrets(credential);
            credential.password_expiry_utc = TIME_MAX;
        }
        if credential.is_full() {
            credential.wwwauth.clear();
            return Ok(());
        }
        if credential.quit {
            return Err(GitError::InvalidFormat(format!(
                "fatal: credential helper '{helper}' told us to quit"
            )));
        }
    }
    if prompt::credential_getpass(config, stack, credential)? || !credential.is_full() {
        return prompt::die_unable_to_get_password();
    }
    Ok(())
}

pub fn credential_approve(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
) -> Result<()> {
    if credential.approved {
        return Ok(());
    }
    if ((!credential.username.is_some() || !credential.password.is_some())
        && credential.credential.is_none())
        || credential.password_expiry_utc < now()
    {
        return Ok(());
    }
    credential_next_state(credential);
    credential_apply_config(config, stack, credential)?;
    for helper in credential.helpers.clone() {
        let _ = credential_do(credential, &helper, "store", false);
    }
    credential.approved = true;
    Ok(())
}

pub fn credential_reject(
    config: Option<&GitConfig>,
    stack: Option<&ConfigStack>,
    credential: &mut GitCredential,
) -> Result<()> {
    credential_next_state(credential);
    credential_apply_config(config, stack, credential)?;
    for helper in credential.helpers.clone() {
        let _ = credential_do(credential, &helper, "erase", false);
    }
    credential_clear_secrets(credential);
    credential.username = None;
    credential.oauth_refresh_token = None;
    credential.password_expiry_utc = TIME_MAX;
    credential.approved = false;
    Ok(())
}

pub fn credential_fill_simple(
    config: Option<&GitConfig>,
    mut request: GitCredential,
) -> Result<Option<GitCredential>> {
    for spec in credential_helper_specs(config) {
        if request.username.is_some() && request.password.is_some() {
            break;
        }
        let helper = spec;
        let mut working = request.clone();
        working.helpers = vec![helper.clone()];
        if credential_do(&mut working, &helper, "get", true).is_ok() {
            if let Some(username) = working.username {
                request.username = Some(username);
            }
            if let Some(password) = working.password {
                request.password = Some(password);
            }
        }
    }
    if request.username.is_some() && request.password.is_some() {
        Ok(Some(request))
    } else {
        Ok(None)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn parse_legacy_git_credential(input: &[u8]) -> Result<GitCredential> {
    crate::parse_legacy_git_credential_impl(input)
}

pub fn encode_legacy_git_credential(credential: &GitCredential) -> Result<Vec<u8>> {
    crate::encode_legacy_git_credential_impl(credential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_defaults_enable_protocol_and_prompt_protection() {
        let credential = GitCredential::default();
        assert!(credential.sanitize_prompt);
        assert!(credential.protect_protocol);
    }

    #[test]
    fn credential_write_rejects_carriage_return_unless_explicitly_disabled() {
        let mut credential = GitCredential {
            protocol: Some("https".into()),
            host: Some("example\r.com".into()),
            ..GitCredential::default()
        };
        let error = credential_write(&credential, &mut Vec::new(), CredentialOpType::Response)
            .expect_err("protected credential must reject a carriage return");
        assert_eq!(
            error,
            GitError::InvalidFormat(
                "fatal: credential value for host contains carriage return\n\
                 If this is intended, set `credential.protectProtocol=false`"
                    .into()
            )
        );

        credential.protect_protocol = false;
        let mut output = Vec::new();
        credential_write(&credential, &mut output, CredentialOpType::Response)
            .expect("explicitly disabled protection must permit a carriage return");
        assert_eq!(output, b"protocol=https\nhost=example\r.com\n");
    }
}
