// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::{GitError, ObjectFormat, Result};
use sley_protocol::*;
use std::io::{Read, Write};
#[cfg(feature = "http-client")]
use std::time::Duration;

pub mod credential;

pub use credential::{
    CredentialOpType, GitCredential, TIME_MAX, cmd_credential_cache, cmd_credential_cache_daemon,
    cmd_credential_store, credential_announce_capabilities, credential_approve, credential_fill,
    credential_next_state, credential_read, credential_reject, credential_set_all_capabilities,
    credential_write,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRequest {
    pub service: GitService,
    pub path: String,
    pub host: Option<String>,
    pub parameters: Vec<String>,
    pub protocol: Option<ProtocolVersion>,
    pub extra_parameters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAnnouncement {
    pub service: GitService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceDiscoveryPayload {
    AdvertisedRefs(RefAdvertisementSet),
    ProtocolV2(TransportHandshake),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDiscoveryResponse {
    pub announcement: ServiceAnnouncement,
    pub payload: ServiceDiscoveryPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitProtocolHeader {
    pub protocol: Option<ProtocolVersion>,
    pub extra_parameters: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTransport {
    Local,
    File,
    Ext,
    Ssh,
    Git,
    Http,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    pub transport: RemoteTransport,
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshCommandVariant {
    OpenSsh,
    Plink,
    TortoisePlink,
    Simple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshIpVersion {
    V4,
    V6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProcessCommand {
    pub program: String,
    pub args: Vec<String>,
}

pub fn parse_service_request(payload: &[u8]) -> Result<ServiceRequest> {
    validate_service_payload("service request", payload)?;
    let (command, parameters) = match payload.iter().position(|byte| *byte == 0) {
        Some(idx) => {
            if !payload.ends_with(&[0]) {
                return Err(GitError::InvalidFormat(
                    "service request parameters must be NUL terminated".into(),
                ));
            }
            (&payload[..idx], Some(&payload[idx + 1..payload.len() - 1]))
        }
        None => (payload, None),
    };
    let command =
        std::str::from_utf8(command).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let (service, path) = command
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidFormat("service request is missing path".into()))?;
    validate_service_field("service request path", path)?;

    let mut request = ServiceRequest {
        service: parse_git_service(service)?,
        path: path.to_string(),
        host: None,
        parameters: Vec::new(),
        protocol: None,
        extra_parameters: Vec::new(),
    };

    let Some(parameters) = parameters else {
        return Ok(request);
    };
    let mut in_extra = false;
    for parameter in parameters.split(|byte| *byte == 0) {
        if parameter.is_empty() {
            in_extra = true;
            continue;
        }
        validate_service_payload("service request parameter", parameter)?;
        let parameter = std::str::from_utf8(parameter)
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        if !in_extra {
            if let Some(host) = parameter.strip_prefix("host=") {
                if request.host.is_some() {
                    return Err(GitError::InvalidFormat(
                        "service request has duplicate host".into(),
                    ));
                }
                validate_service_field("service request host", host)?;
                request.host = Some(host.to_string());
            } else {
                validate_service_field("service request parameter", parameter)?;
                request.parameters.push(parameter.to_string());
            }
            continue;
        }

        if let Some(version) = parameter.strip_prefix("version=") {
            if request.protocol.is_some() {
                return Err(GitError::InvalidFormat(
                    "service request has duplicate protocol version".into(),
                ));
            }
            request.protocol = Some(parse_service_protocol_version(version)?);
        } else {
            validate_service_field("service request extra parameter", parameter)?;
            request.extra_parameters.push(parameter.to_string());
        }
    }
    Ok(request)
}

pub fn encode_service_request(request: &ServiceRequest) -> Result<Vec<u8>> {
    validate_service_field("service request path", &request.path)?;
    let mut out = Vec::new();
    out.extend_from_slice(request.service.as_str().as_bytes());
    out.push(b' ');
    out.extend_from_slice(request.path.as_bytes());

    let has_parameters = request.host.is_some()
        || !request.parameters.is_empty()
        || request.protocol.is_some()
        || !request.extra_parameters.is_empty();
    if has_parameters {
        out.push(0);
    }
    if let Some(host) = &request.host {
        validate_service_field("service request host", host)?;
        out.extend_from_slice(b"host=");
        out.extend_from_slice(host.as_bytes());
        out.push(0);
    }
    for parameter in &request.parameters {
        validate_service_field("service request parameter", parameter)?;
        out.extend_from_slice(parameter.as_bytes());
        out.push(0);
    }
    if request.protocol.is_some() || !request.extra_parameters.is_empty() {
        out.push(0);
        if let Some(protocol) = request.protocol {
            let version = match protocol {
                ProtocolVersion::V0 => {
                    return Err(GitError::InvalidFormat(
                        "service request must not encode protocol v0 as an extra parameter".into(),
                    ));
                }
                ProtocolVersion::V1 => "version=1",
                ProtocolVersion::V2 => "version=2",
            };
            out.extend_from_slice(version.as_bytes());
            out.push(0);
        }
        for parameter in &request.extra_parameters {
            validate_service_field("service request extra parameter", parameter)?;
            out.extend_from_slice(parameter.as_bytes());
            out.push(0);
        }
    }
    Ok(out)
}

pub fn read_service_request(reader: &mut impl Read) -> Result<ServiceRequest> {
    let Some(frame) = read_pkt_line_frame(reader)? else {
        return Err(GitError::InvalidFormat(
            "pkt-line stream ended before service request".into(),
        ));
    };
    match frame {
        PktLineFrame::Data(payload) => parse_service_request(&payload),
        PktLineFrame::Flush | PktLineFrame::Delimiter | PktLineFrame::ResponseEnd => Err(
            GitError::InvalidFormat("service request must be a data packet".into()),
        ),
    }
}

pub fn write_service_request(writer: &mut impl Write, request: &ServiceRequest) -> Result<()> {
    write_pkt_line_payload(writer, &encode_service_request(request)?)
}

pub fn parse_service_announcement(payload: &[u8]) -> Result<ServiceAnnouncement> {
    let text = parse_protocol_v2_line_text("service announcement", payload)?;
    let service = text
        .strip_prefix("# service=")
        .ok_or_else(|| GitError::InvalidFormat("service announcement is missing service".into()))?;
    Ok(ServiceAnnouncement {
        service: parse_git_service(service)?,
    })
}

pub fn encode_service_announcement(announcement: &ServiceAnnouncement) -> Result<Vec<u8>> {
    Ok(line_from_str(&format!(
        "# service={}",
        announcement.service.as_str()
    )))
}

pub fn parse_service_announcement_stream(frames: &[PktLineFrame]) -> Result<ServiceAnnouncement> {
    match frames {
        [PktLineFrame::Data(payload), PktLineFrame::Flush] => parse_service_announcement(payload),
        [PktLineFrame::Data(_), ..] => Err(GitError::InvalidFormat(
            "service announcement stream must contain only announcement and flush".into(),
        )),
        [] => Err(GitError::InvalidFormat(
            "service announcement stream is empty".into(),
        )),
        _ => Err(GitError::InvalidFormat(
            "service announcement stream must start with a data packet".into(),
        )),
    }
}

pub fn encode_service_announcement_stream(
    announcement: &ServiceAnnouncement,
) -> Result<Vec<PktLineFrame>> {
    Ok(vec![
        PktLineFrame::data(encode_service_announcement(announcement)?)?,
        PktLineFrame::Flush,
    ])
}

pub fn read_service_announcement(reader: &mut impl Read) -> Result<ServiceAnnouncement> {
    let frames = read_pkt_line_frames_until_flush(reader)?;
    parse_service_announcement_stream(&frames)
}

pub fn write_service_announcement(
    writer: &mut impl Write,
    announcement: &ServiceAnnouncement,
) -> Result<()> {
    write_pkt_line_payload(writer, &encode_service_announcement(announcement)?)?;
    writer.write_all(b"0000")?;
    Ok(())
}

pub fn parse_service_discovery_response(
    format: ObjectFormat,
    frames: &[PktLineFrame],
) -> Result<ServiceDiscoveryResponse> {
    let announcement_end = frames
        .iter()
        .position(|frame| matches!(frame, PktLineFrame::Flush))
        .ok_or_else(|| {
            GitError::InvalidFormat("service discovery response missing announcement flush".into())
        })?;
    let announcement = parse_service_announcement_stream(&frames[..=announcement_end])?;
    let payload_frames = &frames[announcement_end + 1..];
    if payload_frames.is_empty() {
        return Err(GitError::InvalidFormat(
            "service discovery response missing payload".into(),
        ));
    }
    let payload = match payload_frames.first() {
        Some(PktLineFrame::Data(payload)) if trim_trailing_lf(payload) == b"version 2" => {
            ServiceDiscoveryPayload::ProtocolV2(parse_protocol_v2_advertisement(payload_frames)?)
        }
        Some(_) => ServiceDiscoveryPayload::AdvertisedRefs(parse_ref_advertisement_set(
            format,
            payload_frames,
        )?),
        None => unreachable!("payload_frames is non-empty"),
    };
    Ok(ServiceDiscoveryResponse {
        announcement,
        payload,
    })
}

pub fn encode_service_discovery_response(
    response: &ServiceDiscoveryResponse,
) -> Result<Vec<PktLineFrame>> {
    let mut frames = encode_service_announcement_stream(&response.announcement)?;
    match &response.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(refs) => {
            frames.extend(encode_ref_advertisement_set(refs)?);
        }
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            frames.extend(encode_protocol_v2_advertisement(handshake)?);
        }
    }
    Ok(frames)
}

pub fn read_service_discovery_response(
    format: ObjectFormat,
    reader: &mut impl Read,
) -> Result<ServiceDiscoveryResponse> {
    let mut frames = read_pkt_line_frames_until_flush(reader)?;
    // Smart HTTP with `Git-Protocol: version=2` omits the `# service=` preamble and
    // flush that precede the v2 capability advertisement (upstream http-backend.c).
    if let Some(PktLineFrame::Data(payload)) = frames.first()
        && trim_trailing_lf(payload) == b"version 2"
    {
        return Ok(ServiceDiscoveryResponse {
            announcement: ServiceAnnouncement {
                service: GitService::UploadPack,
            },
            payload: ServiceDiscoveryPayload::ProtocolV2(parse_protocol_v2_advertisement(&frames)?),
        });
    }
    frames.extend(read_pkt_line_frames_until_flush(reader)?);
    parse_service_discovery_response(format, &frames)
}

pub fn write_service_discovery_response(
    writer: &mut impl Write,
    response: &ServiceDiscoveryResponse,
) -> Result<()> {
    write_service_announcement(writer, &response.announcement)?;
    match &response.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(refs) => write_ref_advertisement_set(writer, refs),
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            write_protocol_v2_advertisement(writer, handshake)
        }
    }
}

pub fn parse_remote_url(value: &str) -> Result<RemoteUrl> {
    validate_remote_url_value(value)?;
    if let Some(command) = value.strip_prefix("ext::") {
        if command.is_empty() {
            return Err(GitError::InvalidFormat(
                "ext remote command is empty".into(),
            ));
        }
        validate_remote_path("ext remote command", command)?;
        return Ok(RemoteUrl {
            transport: RemoteTransport::Ext,
            user: None,
            password: None,
            host: None,
            port: None,
            path: command.to_string(),
        });
    }
    if let Some((scheme, rest)) = value.split_once("://") {
        return parse_remote_url_with_scheme(scheme, rest);
    }
    if let Some(colon) = scp_like_separator(value) {
        let (authority, path) = value.split_at(colon);
        let path = &path[1..];
        validate_remote_path("remote path", path)?;
        let (user, host, port) = parse_scp_like_authority(authority)?;
        return Ok(RemoteUrl {
            transport: RemoteTransport::Ssh,
            user,
            password: None,
            host: Some(host),
            port,
            path: path.to_string(),
        });
    }
    validate_remote_path("local remote path", value)?;
    Ok(RemoteUrl {
        transport: RemoteTransport::Local,
        user: None,
        password: None,
        host: None,
        port: None,
        path: value.to_string(),
    })
}

pub fn parse_git_credential(input: &[u8]) -> Result<GitCredential> {
    parse_legacy_git_credential_impl(input)
}

pub fn encode_git_credential(credential: &GitCredential) -> Result<Vec<u8>> {
    encode_legacy_git_credential_impl(credential)
}

pub(crate) fn parse_legacy_git_credential_impl(input: &[u8]) -> Result<GitCredential> {
    let mut credential = GitCredential::default();
    let mut lines = input.split(|byte| *byte == b'\n');
    let mut finished = false;
    for line in lines.by_ref() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            finished = true;
            break;
        }
        let line =
            std::str::from_utf8(line).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let Some((key, value)) = line.split_once('=') else {
            return Err(GitError::InvalidFormat(
                "credential line is missing = delimiter".into(),
            ));
        };
        if key.is_empty() {
            return Err(GitError::InvalidFormat("credential key is empty".into()));
        }
        validate_credential_key(key)?;
        validate_credential_value("credential value", value)?;
        match key {
            "protocol" => set_credential_field(&mut credential.protocol, key, value)?,
            "host" => set_credential_field(&mut credential.host, key, value)?,
            "path" => set_credential_field(&mut credential.path, key, value)?,
            "username" => set_credential_field(&mut credential.username, key, value)?,
            "password" => set_credential_field(&mut credential.password, key, value)?,
            "password_expiry_utc" => {
                if credential.password_expiry_utc != TIME_MAX {
                    return Err(GitError::InvalidFormat(format!(
                        "credential key {key} appears more than once"
                    )));
                }
                credential.password_expiry_utc = value.parse().unwrap_or(TIME_MAX);
            }
            "oauth_refresh_token" => {
                set_credential_field(&mut credential.oauth_refresh_token, key, value)?
            }
            "url" => set_credential_field(&mut credential.url, key, value)?,
            "wwwauth[]" => credential.wwwauth.push(value.to_string()),
            "quit" => credential.quit = !value.is_empty() && value != "0" && value != "false",
            _ => {
                if is_known_credential_key(key) {
                    return Err(GitError::InvalidFormat(format!(
                        "credential key {key} appears more than once"
                    )));
                }
                credential.extra.push((key.to_string(), value.to_string()));
            }
        }
    }
    if !finished {
        return Err(GitError::InvalidFormat(
            "credential payload must end with a blank line".into(),
        ));
    }
    if let Some(rest) = lines.next()
        && (!rest.is_empty() || lines.next().is_some())
    {
        return Err(GitError::InvalidFormat(
            "credential payload has trailing data after blank line".into(),
        ));
    }
    Ok(credential)
}

pub(crate) fn encode_legacy_git_credential_impl(credential: &GitCredential) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_credential_field(&mut out, "protocol", credential.protocol.as_deref())?;
    encode_credential_field(&mut out, "host", credential.host.as_deref())?;
    encode_credential_field(&mut out, "path", credential.path.as_deref())?;
    encode_credential_field(&mut out, "username", credential.username.as_deref())?;
    encode_credential_field(&mut out, "password", credential.password.as_deref())?;
    if credential.password_expiry_utc != TIME_MAX {
        encode_credential_field(
            &mut out,
            "password_expiry_utc",
            Some(&credential.password_expiry_utc.to_string()),
        )?;
    }
    encode_credential_field(
        &mut out,
        "oauth_refresh_token",
        credential.oauth_refresh_token.as_deref(),
    )?;
    encode_credential_field(&mut out, "url", credential.url.as_deref())?;
    for value in &credential.wwwauth {
        encode_credential_field(&mut out, "wwwauth[]", Some(value.as_str()))?;
    }
    if credential.quit {
        encode_credential_field(&mut out, "quit", Some("true"))?;
    }
    for (key, value) in &credential.extra {
        if is_known_credential_key(key) {
            return Err(GitError::InvalidFormat(format!(
                "credential extra key {key} conflicts with a known field"
            )));
        }
        encode_credential_field(&mut out, key, Some(value.as_str()))?;
    }
    out.push(b'\n');
    Ok(out)
}

const MAX_GIT_CREDENTIAL_RESPONSE_BYTES: usize = 64 * 1024;

pub fn read_git_credential(reader: &mut impl Read) -> Result<GitCredential> {
    let mut input = Vec::new();
    reader
        .take((MAX_GIT_CREDENTIAL_RESPONSE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut input)?;
    if input.len() > MAX_GIT_CREDENTIAL_RESPONSE_BYTES {
        return Err(GitError::InvalidFormat(format!(
            "credential helper response exceeds maximum size of {} bytes (64 KiB)",
            MAX_GIT_CREDENTIAL_RESPONSE_BYTES
        )));
    }
    parse_git_credential(&input)
}

pub fn write_git_credential(writer: &mut impl Write, credential: &GitCredential) -> Result<()> {
    credential_write(credential, writer, CredentialOpType::Response)
}

pub fn git_credential_basic_authorization(credential: &GitCredential) -> Result<Option<String>> {
    let (Some(username), Some(password)) = (
        credential.username.as_deref(),
        credential.password.as_deref(),
    ) else {
        return Ok(None);
    };
    validate_http_auth_component("credential username", username)?;
    if username.bytes().any(|byte| byte == b':') {
        return Err(GitError::InvalidFormat(
            "credential username contains a delimiter byte".into(),
        ));
    }
    validate_http_auth_component("credential password", password)?;
    let mut token = Vec::with_capacity(username.len() + 1 + password.len());
    token.extend_from_slice(username.as_bytes());
    token.push(b':');
    token.extend_from_slice(password.as_bytes());
    Ok(Some(format!("Basic {}", encode_base64(&token))))
}

pub fn git_credential_bearer_authorization(token: &str) -> Result<String> {
    validate_http_auth_component("bearer token", token)?;
    if token.is_empty() {
        return Err(GitError::InvalidFormat("bearer token is empty".into()));
    }
    Ok(format!("Bearer {token}"))
}

/// Build the absolute smart-HTTP service-discovery URL for `remote`, e.g.
/// `https://host:443/org/repo.git/info/refs?service=git-upload-pack`.
///
/// The scheme is derived from `remote.transport` (only `Http`/`Https` are
/// accepted). Any userinfo/password embedded in `remote` is intentionally
/// excluded; credentials are supplied separately as request headers.
pub fn http_smart_info_refs_url(remote: &RemoteUrl, service: GitService) -> Result<String> {
    let origin = http_remote_origin(remote)?;
    let path = smart_http_info_refs_path(&remote.path, service)?;
    Ok(format!("{origin}{path}"))
}

/// Build the absolute smart-HTTP RPC URL for `remote`, e.g.
/// `https://host:443/org/repo.git/git-upload-pack`.
///
/// The scheme is derived from `remote.transport` (only `Http`/`Https` are
/// accepted). Any userinfo/password embedded in `remote` is intentionally
/// excluded; credentials are supplied separately as request headers.
pub fn http_smart_rpc_url(remote: &RemoteUrl, service: GitService) -> Result<String> {
    let origin = http_remote_origin(remote)?;
    let path = smart_http_rpc_path(&remote.path, service)?;
    Ok(format!("{origin}{path}"))
}

/// Render the `scheme://host[:port]` origin for an http(s) remote, never
/// including userinfo. IPv6 hosts are bracketed.
fn http_remote_origin(remote: &RemoteUrl) -> Result<String> {
    let scheme = match remote.transport {
        RemoteTransport::Http => "http",
        RemoteTransport::Https => "https",
        _ => {
            return Err(GitError::InvalidFormat(
                "smart HTTP URL requires an http or https remote".into(),
            ));
        }
    };
    let host = remote
        .host
        .as_deref()
        .ok_or_else(|| GitError::InvalidFormat("smart HTTP remote is missing a host".into()))?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match remote.port {
        Some(port) => Ok(format!("{scheme}://{host}:{port}")),
        None => Ok(format!("{scheme}://{host}")),
    }
}

pub fn ssh_service_command(service: GitService, repository_path: &str) -> Result<String> {
    ssh_service_command_with_program(service.as_str(), repository_path)
}

/// Build the remote-shell command for an explicitly selected service program
/// (for example `clone --upload-pack=<path>`). The program is intentionally
/// preserved as a shell command, matching Git's `--upload-pack` contract; the
/// repository argument remains independently validated and shell-quoted.
pub fn ssh_service_command_with_program(program: &str, repository_path: &str) -> Result<String> {
    if program.is_empty()
        || program
            .bytes()
            .any(|byte| matches!(byte, 0 | b'\n' | b'\r'))
    {
        return Err(GitError::InvalidFormat(
            "SSH service program contains an invalid delimiter".into(),
        ));
    }
    validate_ssh_repository_path(repository_path)?;
    let repository_path = ssh_service_repository_path(repository_path);
    Ok(format!(
        "{program} {}",
        quote_ssh_repository_path(repository_path)
    ))
}

pub fn ssh_process_command(
    remote: &RemoteUrl,
    service: GitService,
    program: impl Into<String>,
    variant: SshCommandVariant,
) -> Result<SshProcessCommand> {
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH process command requires an SSH remote".into(),
        ));
    }
    let program = program.into();
    validate_ssh_program(&program)?;
    let args = ssh_process_args(remote, service, variant)?;
    Ok(SshProcessCommand { program, args })
}

pub fn ssh_process_args(
    remote: &RemoteUrl,
    service: GitService,
    variant: SshCommandVariant,
) -> Result<Vec<String>> {
    ssh_process_args_with_ip(remote, service, variant, None)
}

pub fn ssh_process_args_with_ip(
    remote: &RemoteUrl,
    service: GitService,
    variant: SshCommandVariant,
    ip_version: Option<SshIpVersion>,
) -> Result<Vec<String>> {
    ssh_process_args_with_ip_and_command(remote, service, variant, ip_version, None)
}

/// Build SSH argv while allowing the caller to override the remote service
/// program. This is the typed transport seam behind porcelain options such as
/// `clone --upload-pack=<path>`; host, port, IP-family, and repository quoting
/// remain identical to [`ssh_process_args_with_ip`].
pub fn ssh_process_args_with_ip_and_command(
    remote: &RemoteUrl,
    service: GitService,
    variant: SshCommandVariant,
    ip_version: Option<SshIpVersion>,
    service_program: Option<&str>,
) -> Result<Vec<String>> {
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH process arguments require an SSH remote".into(),
        ));
    }
    let mut args = Vec::new();
    if let Some(ip_version) = ip_version {
        match variant {
            SshCommandVariant::OpenSsh
            | SshCommandVariant::Plink
            | SshCommandVariant::TortoisePlink => {
                args.push(
                    match ip_version {
                        SshIpVersion::V4 => "-4",
                        SshIpVersion::V6 => "-6",
                    }
                    .into(),
                );
            }
            SshCommandVariant::Simple => {
                return Err(GitError::InvalidFormat(
                    "simple SSH variant cannot pass an IP version".into(),
                ));
            }
        }
    }
    if matches!(variant, SshCommandVariant::TortoisePlink) {
        args.push("-batch".into());
    }
    if let Some(port) = remote.port {
        match variant {
            SshCommandVariant::OpenSsh => {
                args.push("-p".into());
                args.push(port.to_string());
            }
            SshCommandVariant::Plink | SshCommandVariant::TortoisePlink => {
                args.push("-P".into());
                args.push(port.to_string());
            }
            SshCommandVariant::Simple => {
                return Err(GitError::InvalidFormat(
                    "simple SSH variant cannot pass a port".into(),
                ));
            }
        }
    }
    args.push(ssh_host_argument(remote)?);
    args.push(match service_program {
        Some(program) => ssh_service_command_with_program(program, &remote.path)?,
        None => ssh_service_command(service, &remote.path)?,
    });
    Ok(args)
}

pub fn ssh_host_argument(remote: &RemoteUrl) -> Result<String> {
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH host argument requires an SSH remote".into(),
        ));
    }
    let host = remote
        .host
        .as_deref()
        .ok_or_else(|| GitError::InvalidFormat("SSH remote is missing a host".into()))?;
    validate_remote_host(host)?;
    if let Some(user) = &remote.user {
        validate_ssh_user(user)?;
        Ok(format!("{user}@{host}"))
    } else {
        Ok(host.to_string())
    }
}

pub fn encode_git_protocol_header(header: &GitProtocolHeader) -> Result<Option<String>> {
    let mut fields = Vec::new();
    if let Some(protocol) = header.protocol {
        let version = match protocol {
            ProtocolVersion::V0 => {
                return Err(GitError::InvalidFormat(
                    "Git-Protocol header must not encode protocol v0".into(),
                ));
            }
            ProtocolVersion::V1 => "version=1",
            ProtocolVersion::V2 => "version=2",
        };
        fields.push(version.to_string());
    }
    for parameter in &header.extra_parameters {
        validate_git_protocol_header_parameter(parameter)?;
        fields.push(parameter.clone());
    }
    Ok((!fields.is_empty()).then(|| fields.join(":")))
}

pub fn parse_git_protocol_header(value: &str) -> Result<GitProtocolHeader> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "Git-Protocol header is empty".into(),
        ));
    }
    let mut header = GitProtocolHeader::default();
    for parameter in value.split(':') {
        validate_git_protocol_header_parameter(parameter)?;
        if let Some(version) = parameter.strip_prefix("version=") {
            if header.protocol.is_some() {
                return Err(GitError::InvalidFormat(
                    "Git-Protocol header has duplicate protocol version".into(),
                ));
            }
            header.protocol = Some(parse_service_protocol_version(version)?);
        } else {
            header.extra_parameters.push(parameter.to_string());
        }
    }
    Ok(header)
}

fn validate_remote_url_value(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("remote URL is empty".into()));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "remote URL contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_remote_path(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn set_credential_field(slot: &mut Option<String>, key: &str, value: &str) -> Result<()> {
    if slot.is_some() {
        return Err(GitError::InvalidFormat(format!(
            "credential key {key} appears more than once"
        )));
    }
    *slot = Some(value.to_string());
    Ok(())
}

fn encode_credential_field(out: &mut impl Write, key: &str, value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    validate_credential_key(key)?;
    validate_credential_value("credential value", value)?;
    out.write_all(key.as_bytes())?;
    out.write_all(b"=")?;
    out.write_all(value.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

fn validate_credential_key(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("credential key is empty".into()));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b'=' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "credential key contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_credential_value(label: &str, value: &str) -> Result<()> {
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn is_known_credential_key(value: &str) -> bool {
    matches!(
        value,
        "protocol"
            | "host"
            | "path"
            | "username"
            | "password"
            | "password_expiry_utc"
            | "oauth_refresh_token"
            | "url"
            | "wwwauth[]"
            | "quit"
    )
}

fn validate_http_auth_component(label: &str, value: &str) -> Result<()> {
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(first >> 2) as usize] as char);
        out.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn parse_remote_url_with_scheme(scheme: &str, rest: &str) -> Result<RemoteUrl> {
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "file" => {
            validate_remote_path("file remote path", rest)?;
            if !rest.starts_with('/') {
                return Err(GitError::InvalidFormat(
                    "file remote path must start with /".into(),
                ));
            }
            Ok(RemoteUrl {
                transport: RemoteTransport::File,
                user: None,
                password: None,
                host: None,
                port: None,
                path: rest.to_string(),
            })
        }
        "ssh" | "git+ssh" | "ssh+git" | "git" | "http" | "https" => {
            let is_http = scheme == "http" || scheme == "https";
            let empty_port_is_absent = matches!(scheme.as_str(), "ssh" | "git+ssh" | "ssh+git");
            let (authority, path) = split_remote_authority_and_path(rest)?;
            // Only http(s) userinfo may carry an embedded password; SSH/git keep
            // their authority verbatim so existing behavior does not regress.
            let (user, password, host, port) =
                if matches!(scheme.as_str(), "ssh" | "git+ssh" | "ssh+git") {
                    let (user, host, port) = parse_ssh_scheme_authority(authority)?;
                    (user, None, host, port)
                } else {
                    parse_remote_authority(authority, true, is_http, empty_port_is_absent)?
                };
            let path = if matches!(scheme.as_str(), "ssh" | "git+ssh" | "ssh+git") {
                percent_decode_remote_path(&path)?
            } else {
                path
            };
            Ok(RemoteUrl {
                transport: match scheme.as_str() {
                    "ssh" | "git+ssh" | "ssh+git" => RemoteTransport::Ssh,
                    "git" => RemoteTransport::Git,
                    "http" => RemoteTransport::Http,
                    "https" => RemoteTransport::Https,
                    _ => unreachable!("matched remote URL scheme"),
                },
                user,
                password,
                host: Some(host),
                port,
                path,
            })
        }
        _ => Err(GitError::InvalidFormat(format!(
            "unsupported remote URL scheme {scheme}"
        ))),
    }
}

fn split_remote_authority_and_path(value: &str) -> Result<(&str, String)> {
    let slash = value
        .find('/')
        .ok_or_else(|| GitError::InvalidFormat("remote URL is missing a path".into()))?;
    let (authority, path) = value.split_at(slash);
    if authority.is_empty() {
        return Err(GitError::InvalidFormat(
            "remote URL is missing a host".into(),
        ));
    }
    validate_remote_path("remote path", path)?;
    Ok((authority, path.to_string()))
}

fn percent_decode_remote_path(value: &str) -> Result<String> {
    let decoded = sley_core::text::percent_decode(value.as_bytes()).map_err(|_| {
        GitError::InvalidFormat(format!("invalid percent-encoded remote path {value:?}"))
    })?;
    let decoded =
        String::from_utf8(decoded).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_remote_path("remote path", &decoded)?;
    Ok(decoded)
}

/// Parsed remote authority: `(user, password, host, port)`. Password is only
/// ever populated for http(s) authorities (see `parse_remote_authority`).
type ParsedAuthority = (Option<String>, Option<String>, String, Option<u16>);

fn parse_remote_authority(
    value: &str,
    allow_port: bool,
    split_password: bool,
    empty_port_is_absent: bool,
) -> Result<ParsedAuthority> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "remote URL is missing a host".into(),
        ));
    }
    let (user, password, host_port) = match value.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            if userinfo.is_empty() {
                return Err(GitError::InvalidFormat("remote URL user is empty".into()));
            }
            if split_password {
                // For http(s) URLs the userinfo may embed a password as
                // "user:pass"; split on the first ':' so the password can be
                // surfaced separately and kept out of derived URLs.
                match userinfo.split_once(':') {
                    Some((user, password)) => (
                        Some(user.to_string()),
                        Some(password.to_string()),
                        host_port,
                    ),
                    None => (Some(userinfo.to_string()), None, host_port),
                }
            } else {
                // SSH/scp-like authorities keep the userinfo verbatim, matching
                // existing behavior (no embedded-password concept).
                (Some(userinfo.to_string()), None, host_port)
            }
        }
        None => (None, None, value),
    };
    let (host, port) = parse_remote_host_port(host_port, allow_port, empty_port_is_absent)?;
    validate_remote_host(&host)?;
    Ok((user, password, host, port))
}

fn parse_ssh_scheme_authority(value: &str) -> Result<(Option<String>, String, Option<u16>)> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "remote URL is missing a host".into(),
        ));
    }
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| GitError::InvalidFormat("remote URL IPv6 host is missing ]".into()))?;
        let bracketed = &rest[..end];
        let suffix = &rest[end + 1..];
        let (user, host) = split_ssh_user_host(bracketed)?;
        let port = parse_optional_ssh_port_suffix(suffix)?;
        validate_remote_host(host)?;
        return Ok((user.map(str::to_string), host.to_string(), port));
    }

    let (user, host_port) = match value.rsplit_once('@') {
        Some((userinfo, host_port)) => {
            if userinfo.is_empty() {
                return Err(GitError::InvalidFormat("remote URL user is empty".into()));
            }
            (Some(userinfo), host_port)
        }
        None => (None, value),
    };
    validate_optional_ssh_user(user)?;

    if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| GitError::InvalidFormat("remote URL IPv6 host is missing ]".into()))?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = parse_optional_ssh_port_suffix(suffix)?;
        validate_remote_host(host)?;
        return Ok((user.map(str::to_string), host.to_string(), port));
    }

    if host_port.contains(']') {
        return Err(GitError::InvalidFormat(
            "remote URL has invalid bracketed host".into(),
        ));
    }
    let (host, port) = if host_port.matches(':').count() <= 1 {
        match host_port.rsplit_once(':') {
            Some((host, "")) => (host, None),
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                (host, Some(parse_remote_port(port)?))
            }
            Some((_, _)) => {
                return Err(GitError::InvalidFormat(
                    "remote URL port must be numeric".into(),
                ));
            }
            None => (host_port, None),
        }
    } else {
        (host_port, None)
    };
    validate_remote_host(host)?;
    Ok((user.map(str::to_string), host.to_string(), port))
}

fn split_ssh_user_host(value: &str) -> Result<(Option<&str>, &str)> {
    match value.rsplit_once('@') {
        Some((user, host)) => {
            if user.is_empty() {
                return Err(GitError::InvalidFormat("remote URL user is empty".into()));
            }
            validate_optional_ssh_user(Some(user))?;
            Ok((Some(user), host))
        }
        None => Ok((None, value)),
    }
}

fn validate_optional_ssh_user(user: Option<&str>) -> Result<()> {
    if let Some(user) = user {
        validate_ssh_user(user)?;
    }
    Ok(())
}

fn parse_optional_ssh_port_suffix(suffix: &str) -> Result<Option<u16>> {
    if suffix.is_empty() || suffix == ":" {
        return Ok(None);
    }
    let Some(port) = suffix.strip_prefix(':') else {
        return Err(GitError::InvalidFormat(
            "remote URL has invalid bracketed host suffix".into(),
        ));
    };
    Ok(Some(parse_remote_port(port)?))
}

fn parse_remote_host_port(
    value: &str,
    allow_port: bool,
    empty_port_is_absent: bool,
) -> Result<(String, Option<u16>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| GitError::InvalidFormat("remote URL IPv6 host is missing ]".into()))?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = if let Some(port) = suffix.strip_prefix(':') {
            if !allow_port {
                return Err(GitError::InvalidFormat(
                    "remote URL must not include a port".into(),
                ));
            }
            if port.is_empty() && empty_port_is_absent {
                return Ok((host.to_string(), None));
            }
            Some(parse_remote_port(port)?)
        } else if suffix.is_empty() {
            None
        } else {
            return Err(GitError::InvalidFormat(
                "remote URL has invalid bracketed host suffix".into(),
            ));
        };
        return Ok((host.to_string(), port));
    }
    if value.contains(']') {
        return Err(GitError::InvalidFormat(
            "remote URL has invalid bracketed host".into(),
        ));
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        if value[..host.len()].contains(':') {
            return Err(GitError::InvalidFormat(
                "remote URL IPv6 host must be bracketed".into(),
            ));
        }
        if port.is_empty() && empty_port_is_absent {
            return Ok((host.to_string(), None));
        }
        if port.bytes().all(|byte| byte.is_ascii_digit()) {
            if !allow_port {
                return Err(GitError::InvalidFormat(
                    "remote URL must not include a port".into(),
                ));
            }
            return Ok((host.to_string(), Some(parse_remote_port(port)?)));
        }
        return Err(GitError::InvalidFormat(
            "remote URL port must be numeric".into(),
        ));
    }
    Ok((value.to_string(), None))
}

fn parse_scp_like_authority(value: &str) -> Result<(Option<String>, String, Option<u16>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| GitError::InvalidFormat("remote URL IPv6 host is missing ]".into()))?;
        if end + 1 != rest.len() {
            return Err(GitError::InvalidFormat(
                "remote URL has invalid bracketed host suffix".into(),
            ));
        }
        let bracketed = &rest[..end];
        let (user, host_port) = match bracketed.rsplit_once('@') {
            Some((userinfo, host_port)) => {
                if userinfo.is_empty() {
                    return Err(GitError::InvalidFormat("remote URL user is empty".into()));
                }
                (Some(userinfo.to_string()), host_port)
            }
            None => (None, bracketed),
        };
        let (host, port) = split_scp_like_bracketed_host_port(host_port)?;
        validate_remote_host(&host)?;
        return Ok((user, host, port));
    }
    let (user, _password, host, port) = parse_remote_authority(value, false, false, false)?;
    if port.is_some() {
        return Err(GitError::InvalidFormat(
            "scp-like SSH remote must not include a port".into(),
        ));
    }
    Ok((user, host, None))
}

fn split_scp_like_bracketed_host_port(value: &str) -> Result<(String, Option<u16>)> {
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.contains(':')
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Ok((host.to_string(), Some(parse_remote_port(port)?)));
    }
    Ok((value.to_string(), None))
}

fn parse_remote_port(value: &str) -> Result<u16> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("remote URL port is empty".into()));
    }
    value
        .parse::<u16>()
        .map_err(|err| GitError::InvalidFormat(err.to_string()))
}

fn validate_remote_host(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("remote URL host is empty".into()));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "remote URL host contains a delimiter byte".into(),
        ));
    }
    Ok(())
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

fn validate_ssh_repository_path(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "SSH repository path is empty".into(),
        ));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "SSH repository path contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_ssh_program(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("SSH program is empty".into()));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(
            "SSH program contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_ssh_user(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat("SSH user is empty".into()));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "SSH user contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn quote_ssh_repository_path(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn ssh_service_repository_path(value: &str) -> &str {
    if value.starts_with("/~") {
        &value[1..]
    } else {
        value
    }
}

fn validate_git_protocol_header_parameter(value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(
            "Git-Protocol header parameter is empty".into(),
        ));
    }
    if value
        .bytes()
        .any(|byte| matches!(byte, b':' | b'\n' | b'\r' | 0))
    {
        return Err(GitError::InvalidFormat(
            "Git-Protocol header parameter contains a delimiter byte".into(),
        ));
    }
    Ok(())
}

fn validate_service_payload(label: &str, value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.iter().any(|byte| matches!(*byte, b'\n' | b'\r')) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn validate_service_field(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn parse_service_protocol_version(value: &str) -> Result<ProtocolVersion> {
    match value {
        "1" => Ok(ProtocolVersion::V1),
        "2" => Ok(ProtocolVersion::V2),
        other => Err(GitError::InvalidFormat(format!(
            "unsupported service request protocol version {other}"
        ))),
    }
}

/// User-Agent advertised by the built-in HTTP transport client.
#[cfg(feature = "http-client")]
pub const HTTP_USER_AGENT: &str = "git/2.54.0 (sley)";

/// A buffered HTTP response whose body streams directly from the network.
///
/// Note that *any* HTTP status (including 4xx/5xx) is reported here with a
/// populated [`status`](HttpResponse::status); transport-level failures are
/// reported as [`Err`] from the [`HttpClient`] methods instead.
#[cfg(feature = "http-client")]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub body: Box<dyn std::io::Read + Send>,
}

/// Minimal byte-transport over HTTP(S) used to drive smart-HTTP git transport.
///
/// This is the injectable seam through which a host enforces network policy: an
/// implementation owns the entire dial (DNS resolution, connect, TLS) for each
/// `url`, so a host mirroring attacker-controlled public URLs can supply a client
/// that validates the resolved IP and pins the connection to it, guarding against
/// SSRF. The default fetch/clone path uses [`UreqHttpClient`]; see
/// `sley_remote::fetch_with_http_client` / `clone_with_http_client` to inject one.
///
/// Implementations must surface HTTP error statuses (401/403/404/5xx) as
/// `Ok(HttpResponse { status, .. })` so callers can react to them (for example,
/// retrying a 401 with credentials). Only genuine transport failures
/// (DNS/connect/TLS/timeout/protocol) are reported as `Err`.
#[cfg(feature = "http-client")]
pub trait HttpClient {
    /// Issue a `GET` for `url`, sending the additional `headers`.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse>;

    /// Download a protocol-v2 packfile URI. The default is a normal streaming
    /// GET; the built-in client overrides this to use parallel byte ranges when
    /// the origin supports them.
    fn get_packfile(&self, url: &str) -> Result<HttpResponse> {
        self.get(url, &[])
    }

    /// Issue a `POST` for `url` with `body`, sending `content_type` and the
    /// additional `headers`.
    fn post(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse>;

    /// Issue a `POST` whose body is streamed from `body` with chunked
    /// transfer-encoding (no `Content-Length`), so large request bodies never
    /// have to be held in memory. The default implementation buffers `body` and
    /// delegates to [`HttpClient::post`]; transports that can stream the request
    /// (e.g. [`UreqHttpClient`]) override this. Callers that need retry-on-auth
    /// must be able to regenerate `body` per attempt, since a reader is consumed
    /// once.
    fn post_reader(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &mut dyn std::io::Read,
    ) -> Result<HttpResponse> {
        let buffered = read_to_end_bounded(body, self.limits().http_request_body())?;
        self.post(url, content_type, headers, &buffered)
    }

    /// The ceilings this client applies to bodies it buffers whole.
    ///
    /// The default is [`TransportLimits::default`], so a client that does
    /// not override this behaves exactly as it did before the limits became
    /// configurable. [`UreqHttpClient`] overrides it with the limits it was
    /// built from, which is what keeps a configured size ceiling and the
    /// deadline derived from it in agreement.
    fn limits(&self) -> TransportLimits {
        TransportLimits::default()
    }
}

/// [`HttpClient`] backed by [`ureq`] with rustls + bundled Mozilla roots.
#[cfg(feature = "http-client")]
pub struct UreqHttpClient {
    agent: ureq::Agent,
    limits: TransportLimits,
}

/// Max time for the DNS lookup.
///
/// glibc's resolver waits 5s per attempt and makes two attempts, so a lookup
/// that has not answered inside 10s is not going to.
#[cfg(feature = "http-client")]
const HTTP_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Max time to establish the connection (TCP handshake, proxy, TLS).
///
/// Linux retransmits an unanswered SYN at 1s, 3s, 7s and 15s; 20s admits the
/// first three retransmits, so a single lossy handshake still succeeds. curl's
/// 300s default -- which git inherits -- does not bound anything useful here.
#[cfg(feature = "http-client")]
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Max time to send the request headers (not the body).
///
/// A few hundred bytes. A peer that cannot absorb them in 20s is stalled.
#[cfg(feature = "http-client")]
const HTTP_SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Max time to await a `100 Continue`.
///
/// ureq's own default, kept as-is: a peer that never sends one is expected, and
/// the request proceeds without it.
#[cfg(feature = "http-client")]
const HTTP_AWAIT_100_TIMEOUT: Duration = Duration::from_secs(1);

/// Max time to send or to receive a body.
///
/// Derived rather than chosen: the largest body sley will buffer divided by the
/// slowest average rate it will serve -- both fields of the same
/// [`TransportLimits`] value, so the size ceiling and the time ceiling cannot
/// drift apart. Raising the ceiling raises the deadline that pays for it,
/// without a second knob to keep in step. 4 GiB at 1 MiB/s is 4096s, a little
/// over an hour.
///
/// [`TransportLimits::body_transfer_timeout`] clamps the result to
/// `sley_protocol::MAX_BODY_TRANSFER_TIMEOUT`, so no configured combination of
/// ceiling and rate can derive a deadline long enough to stop being one.
///
/// ureq's timeouts are whole-phase deadlines, not idle timeouts, so this bounds
/// the total transfer rather than the gap between reads. A peer trickling one
/// byte per interval is therefore cut off here, while a peer on a merely slow
/// link is refused only if the transfer would not have finished in the budget.
#[cfg(feature = "http-client")]
fn http_body_timeout(limits: TransportLimits) -> Duration {
    limits.body_transfer_timeout()
}

/// Max time for the whole call, redirects included: the sum of every phase
/// above. A redirect chain shares this one budget instead of getting a fresh
/// deadline per hop.
#[cfg(feature = "http-client")]
fn http_global_timeout(limits: TransportLimits) -> Duration {
    Duration::from_secs(
        HTTP_RESOLVE_TIMEOUT.as_secs()
            + HTTP_CONNECT_TIMEOUT.as_secs()
            + HTTP_SEND_REQUEST_TIMEOUT.as_secs()
            + HTTP_AWAIT_100_TIMEOUT.as_secs()
            + 2 * http_body_timeout(limits).as_secs(),
    )
}

/// Every deadline applied to an outbound HTTP request.
///
/// ureq 3.3.0's `Timeouts::default()` leaves every field `None` except
/// `await_100`, which is to say there is no connect, read, write or overall
/// deadline at all: a peer that accepts and then stalls holds the request open
/// in unbounded wall-clock, and a byte ceiling never fires because the volume
/// stays small (sley#163). Every field is named explicitly here, so a field
/// added to ureq later cannot silently reintroduce an unbounded phase.
#[cfg(feature = "http-client")]
fn http_timeouts(limits: TransportLimits) -> ureq::config::Timeouts {
    let body = http_body_timeout(limits);
    let global = http_global_timeout(limits);
    ureq::config::Timeouts {
        global: Some(global),
        per_call: Some(global),
        resolve: Some(HTTP_RESOLVE_TIMEOUT),
        connect: Some(HTTP_CONNECT_TIMEOUT),
        send_request: Some(HTTP_SEND_REQUEST_TIMEOUT),
        await_100: Some(HTTP_AWAIT_100_TIMEOUT),
        send_body: Some(body),
        // ureq checks `recv_response` again throughout RecvBody, so a short
        // header-only value here truncates large healthy bodies. Give both
        // receive states the body budget. While awaiting headers ureq also
        // checks the preceding `send_request` deadline, preserving the tighter
        // 20-second stalled-header bound.
        recv_response: Some(body),
        recv_body: Some(body),
    }
}

#[cfg(feature = "http-client")]
fn ureq_agent(
    timeouts: ureq::config::Timeouts,
    tls_config: Option<ureq::tls::TlsConfig>,
) -> ureq::Agent {
    // `http_status_as_error(false)` makes ureq deliver 4xx/5xx as a normal
    // response (carrying status + body) rather than an error, which is what
    // smart-HTTP callers need (e.g. inspecting 401 to prompt for creds).
    //
    // `max_redirects(0)` disables automatic redirect following so each
    // Location can be checked against the configured protocol allow-list.
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(HTTP_USER_AGENT)
        .timeout_global(timeouts.global)
        .timeout_per_call(timeouts.per_call)
        .timeout_resolve(timeouts.resolve)
        .timeout_connect(timeouts.connect)
        .timeout_send_request(timeouts.send_request)
        .timeout_await_100(timeouts.await_100)
        .timeout_send_body(timeouts.send_body)
        .timeout_recv_response(timeouts.recv_response)
        .timeout_recv_body(timeouts.recv_body);
    if let Some(tls_config) = tls_config {
        builder = builder.tls_config(tls_config);
    }
    builder.build().into()
}

#[cfg(feature = "http-client")]
impl UreqHttpClient {
    /// A client with the default ceilings and the deadlines derived from
    /// them -- the behaviour every caller got before the ceilings became
    /// configurable.
    pub fn new() -> Self {
        Self::with_limits(TransportLimits::default())
    }

    /// A client whose buffered-response ceilings, and the body deadlines
    /// derived from them, come from `limits`.
    ///
    /// `limits` is clamped on the way in, so this cannot build a client with
    /// an unbounded read or an unbounded wait however it is called.
    pub fn with_limits(limits: TransportLimits) -> Self {
        Self::with_limits_and_tls_config(limits, ureq_tls_config())
    }

    /// Build a rustls client that augments the bundled Mozilla roots with a PEM CA
    /// certificate bundle supplied by the embedding application.
    #[cfg(feature = "tls-rustls")]
    pub fn with_extra_ca_certificate_pem(ca_pem: &[u8]) -> Result<Self> {
        use ureq::tls::{PemItem, RootCerts, TlsConfig, TlsProvider};

        let mut certificates = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|certificate| ureq::tls::Certificate::from_der(certificate.as_ref()).to_owned())
            .collect::<Vec<_>>();
        let mut extra_certificate_count = 0;
        for item in ureq::tls::parse_pem(ca_pem) {
            if let PemItem::Certificate(certificate) = item.map_err(|error| {
                GitError::InvalidFormat(format!("invalid TLS CA certificate bundle: {error}"))
            })? {
                extra_certificate_count += 1;
                certificates.push(certificate);
            }
        }
        if extra_certificate_count == 0 {
            return Err(GitError::InvalidFormat(
                "TLS CA certificate bundle contains no certificates".to_string(),
            ));
        }
        let tls_config = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::from(certificates))
            .build();
        Ok(Self::with_limits_and_tls_config(
            TransportLimits::default(),
            Some(tls_config),
        ))
    }

    fn with_limits_and_tls_config(
        limits: TransportLimits,
        tls_config: Option<ureq::tls::TlsConfig>,
    ) -> Self {
        let limits = limits.clamped();
        Self {
            agent: ureq_agent(http_timeouts(limits), tls_config),
            limits,
        }
    }
}

/// Build explicit TLS settings when a non-default backend is selected.
#[cfg(feature = "http-client")]
fn ureq_tls_config() -> Option<ureq::tls::TlsConfig> {
    #[cfg(feature = "tls-platform-verifier")]
    {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        Some(
            TlsConfig::builder()
                .provider(TlsProvider::Rustls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
    }
    #[cfg(all(feature = "tls-native-tls", not(feature = "tls-platform-verifier")))]
    {
        use ureq::tls::{TlsConfig, TlsProvider};
        Some(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .build(),
        )
    }
    #[cfg(not(any(
        feature = "tls-platform-verifier",
        all(feature = "tls-native-tls", not(feature = "tls-platform-verifier"))
    )))]
    {
        // `tls-rustls` (and the default ureq rustls stack) need no explicit config.
        None
    }
}

#[cfg(feature = "http-client")]
impl Default for UreqHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "http-client")]
impl HttpClient for UreqHttpClient {
    fn limits(&self) -> TransportLimits {
        self.limits
    }

    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        self.request_with_redirects("GET", url, headers, None)
    }

    fn get_packfile(&self, url: &str) -> Result<HttpResponse> {
        const MIN_PARALLEL_BYTES: u64 = 4 * 1024 * 1024;
        const RANGE_CONNECTIONS: u64 = 8;

        let probe = self.get(url, &[("Range", "bytes=0-0")])?;
        if probe.status != 206 {
            // A server that ignores Range normally returns the complete body as
            // 200, so preserve that already-open streaming response.
            return Ok(probe);
        }
        let Some(total) = probe
            .content_range
            .as_deref()
            .and_then(parse_content_range_total)
        else {
            return self.get(url, &[]);
        };
        if total < MIN_PARALLEL_BYTES {
            return self.get(url, &[]);
        }
        drop(probe);

        let range_count = RANGE_CONNECTIONS.min(total);
        let range_size = total.div_ceil(range_count);
        let mut ranges = Vec::new();
        let mut start = 0_u64;
        while start < total {
            let end = start.saturating_add(range_size).min(total) - 1;
            ranges.push((start, end));
            start = end + 1;
        }

        let mut parts = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(ranges.len());
            for (index, (start, end)) in ranges.into_iter().enumerate() {
                let client = UreqHttpClient {
                    agent: self.agent.clone(),
                    limits: self.limits,
                };
                handles.push(
                    scope.spawn(move || download_packfile_range(client, url, index, start, end)),
                );
            }
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        GitError::Io(format!(
                            "HTTP range request to {url} failed: worker panicked"
                        ))
                    })?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        parts.sort_by_key(|(index, _)| *index);
        Ok(HttpResponse {
            status: 200,
            content_type: Some("application/x-git-packed-objects".into()),
            content_length: Some(total),
            content_range: None,
            body: Box::new(RangePartsReader {
                parts: parts.into_iter().map(|(_, file)| file).collect(),
                current: 0,
            }),
        })
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse> {
        // Only the initial request carries a body; redirect hops are GETs
        // (git/curl drop the body on 302/303-style hops for upload).
        let mut headers_with_ct: Vec<(&str, &str)> = Vec::with_capacity(headers.len() + 1);
        headers_with_ct.push(("Content-Type", content_type));
        headers_with_ct.extend_from_slice(headers);
        self.request_with_redirects("POST", url, &headers_with_ct, Some(body))
    }

    fn post_reader(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &mut dyn std::io::Read,
    ) -> Result<HttpResponse> {
        // Streaming posts cannot rewind for multi-hop redirect body replay;
        // follow redirects only after the first response (body already sent).
        trace_curl_request("POST", url, headers, true);
        let mut request = self.agent.post(url).header("Content-Type", content_type);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        // `SendBody::from_reader` carries no known length, so ureq sends the
        // request with `Transfer-Encoding: chunked` and pulls bytes on demand.
        // Initial POST is from_user; check before dial.
        check_http_layer_scheme_allowed(url, true)?;
        let response = request
            .send(ureq::SendBody::from_reader(body))
            .map_err(|err| http_transport_error(url, &err))?;
        let parts = http_response_parts_from_ureq(response);
        if (300..400).contains(&parts.status) {
            let next = resolve_redirect_url(url, parts.location.as_deref())?;
            // Subsequent hops are GETs without a body (body already consumed)
            // and are not from-user (CURLOPT_REDIR_PROTOCOLS).
            check_http_layer_scheme_allowed(&next, false)?;
            return self.request_with_redirects_from_user("GET", &next, headers, None, false);
        }
        Ok(HttpResponse {
            status: parts.status,
            content_type: parts.content_type,
            content_length: parts.content_length,
            content_range: parts.content_range,
            body: parts.body,
        })
    }
}

#[cfg(feature = "http-client")]
fn parse_content_range_total(value: &str) -> Option<u64> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    if start != "0" || end != "0" {
        return None;
    }
    total.parse().ok().filter(|total| *total > 0)
}

#[cfg(feature = "http-client")]
fn download_packfile_range(
    client: UreqHttpClient,
    url: &str,
    index: usize,
    start: u64,
    end: u64,
) -> Result<(usize, std::fs::File)> {
    use std::io::{Seek, SeekFrom};

    let range = format!("bytes={start}-{end}");
    let mut response = client.get(url, &[("Range", &range)])?;
    if response.status != 206 {
        return Err(GitError::Io(format!(
            "HTTP range request to {url} returned status {}",
            response.status
        )));
    }
    let expected = end - start + 1;
    let mut file = tempfile::tempfile()?;
    let written = std::io::copy(&mut response.body.by_ref().take(expected + 1), &mut file)?;
    if written != expected {
        return Err(GitError::Io(format!(
            "HTTP range request to {url} returned {written} bytes, expected {expected}"
        )));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok((index, file))
}

#[cfg(feature = "http-client")]
struct RangePartsReader {
    parts: Vec<std::fs::File>,
    current: usize,
}

#[cfg(feature = "http-client")]
impl Read for RangePartsReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while let Some(part) = self.parts.get_mut(self.current) {
            let count = part.read(buffer)?;
            if count != 0 {
                return Ok(count);
            }
            self.current += 1;
        }
        Ok(0)
    }
}

#[cfg(feature = "http-client")]
impl UreqHttpClient {
    /// Issue `method` against `url`, following redirects while enforcing the
    /// transport-protocol allow list (GIT_ALLOW_PROTOCOL / protocol.*.allow).
    fn request_with_redirects(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse> {
        // Initial request is from_user (CURLOPT_PROTOCOLS).
        self.request_with_redirects_from_user(method, url, headers, body, true)
    }

    fn request_with_redirects_from_user(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        initial_from_user: bool,
    ) -> Result<HttpResponse> {
        const MAX_REDIRECTS: usize = 20;
        let mut current = url.to_string();
        let mut method = method.to_string();
        let mut body = body.map(|b| b.to_vec());
        // Initial request: from_user=true (CURLOPT_PROTOCOLS). Redirect hops:
        // from_user=false (CURLOPT_REDIR_PROTOCOLS). That is how
        // protocol.http.allow=user blocks smart-redir-perm (t5812).
        let mut from_user = initial_from_user;
        for _ in 0..=MAX_REDIRECTS {
            check_http_layer_scheme_allowed(&current, from_user)?;
            let chunked = false;
            trace_curl_request(&method, &current, headers, chunked);
            let response = match method.as_str() {
                "GET" => {
                    let mut request = self.agent.get(&current);
                    for (name, value) in headers {
                        // Content-Type on GET is harmless but skip if present.
                        if name.eq_ignore_ascii_case("Content-Type") {
                            continue;
                        }
                        request = request.header(*name, *value);
                    }
                    request
                        .call()
                        .map_err(|err| http_transport_error(&current, &err))?
                }
                "POST" => {
                    let mut request = self.agent.post(&current);
                    for (name, value) in headers {
                        request = request.header(*name, *value);
                    }
                    let payload = body.as_deref().unwrap_or(b"");
                    request
                        .send(payload)
                        .map_err(|err| http_transport_error(&current, &err))?
                }
                other => {
                    return Err(GitError::Io(format!(
                        "HTTP request to {current} failed: unsupported method {other}"
                    )));
                }
            };
            let parts = http_response_parts_from_ureq(response);
            if (300..400).contains(&parts.status) {
                let next = resolve_redirect_url(&current, parts.location.as_deref())?;
                // 303 and (for POST) 302 become GET without body, matching curl.
                if method == "POST" && (parts.status == 302 || parts.status == 303) {
                    method = "GET".to_string();
                    body = None;
                }
                current = next;
                from_user = false;
                continue;
            }
            return Ok(HttpResponse {
                status: parts.status,
                content_type: parts.content_type,
                content_length: parts.content_length,
                content_range: parts.content_range,
                body: parts.body,
            });
        }
        Err(GitError::Io(format!(
            "HTTP request to {url} failed: too many redirects"
        )))
    }
}

/// Split a ureq response into status / content-type / Location / body reader.
#[cfg(feature = "http-client")]
struct HttpResponseParts {
    status: u16,
    content_type: Option<String>,
    content_length: Option<u64>,
    content_range: Option<String>,
    location: Option<String>,
    body: Box<dyn std::io::Read + Send>,
}

#[cfg(feature = "http-client")]
fn http_response_parts_from_ureq(response: ureq::http::Response<ureq::Body>) -> HttpResponseParts {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(ureq::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let location = response
        .headers()
        .get(ureq::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let content_length = response
        .headers()
        .get(ureq::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let content_range = response
        .headers()
        .get(ureq::http::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.into_body().into_reader();
    HttpResponseParts {
        status,
        content_type,
        content_length,
        content_range,
        location,
        body: Box::new(body),
    }
}

/// Resolve a (possibly relative) Location header against the request URL.
#[cfg(feature = "http-client")]
fn resolve_redirect_url(current: &str, location: Option<&str>) -> Result<String> {
    let Some(location) = location.filter(|value| !value.is_empty()) else {
        return Err(GitError::Io(format!(
            "HTTP request to {current} failed: redirect with no Location"
        )));
    };
    if location.contains("://") {
        return Ok(location.to_string());
    }
    // Relative redirect: join against the current URL's origin + directory.
    if let Some(scheme_end) = current.find("://") {
        let after_scheme = &current[scheme_end + 3..];
        if location.starts_with('/') {
            let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
            return Ok(format!(
                "{}{}",
                &current[..scheme_end + 3 + host_end],
                location
            ));
        }
        if let Some(slash) = current.rfind('/') {
            return Ok(format!("{}{location}", &current[..=slash]));
        }
    }
    Ok(location.to_string())
}

/// Enforce GIT_ALLOW_PROTOCOL / protocol.<scheme>.allow for a URL the HTTP
/// layer is about to dial.
///
/// `from_user` mirrors curl's dual allow-lists: the initial request uses
/// `from_user=true` (CURLOPT_PROTOCOLS); redirect hops use `from_user=false`
/// (CURLOPT_REDIR_PROTOCOLS). Error text mirrors curl's
/// `Protocol "ftp" not supported or disabled in libcurl` so t5812's
/// `ftp.*disabled` grep matches.
#[cfg(feature = "http-client")]
fn check_http_layer_scheme_allowed(url: &str, from_user: bool) -> Result<()> {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if scheme.is_empty() {
        return Ok(());
    }
    if http_layer_scheme_allowed(&scheme, from_user) {
        return Ok(());
    }
    Err(GitError::Io(format!(
        "Protocol \"{scheme}\" not supported or disabled in libcurl"
    )))
}

#[cfg(feature = "http-client")]
#[derive(Clone, Copy)]
enum HttpProtocolAllow {
    Never,
    UserOnly,
    Always,
}

/// Mirror of git's `get_curl_allowed_protocols` + `is_transport_allowed` for the
/// schemes curl's HTTP layer can dial (http/https/ftp/ftps).
#[cfg(feature = "http-client")]
fn http_layer_scheme_allowed(scheme: &str, from_user: bool) -> bool {
    if let Ok(allow) = std::env::var("GIT_ALLOW_PROTOCOL") {
        return allow
            .split(':')
            .any(|entry| entry.eq_ignore_ascii_case(scheme));
    }
    match http_layer_protocol_policy(scheme) {
        HttpProtocolAllow::Always => true,
        HttpProtocolAllow::Never => false,
        HttpProtocolAllow::UserOnly => from_user,
    }
}

/// Resolve protocol.<scheme>.allow / protocol.allow, including `-c` values
/// folded into `GIT_CONFIG_PARAMETERS`.
#[cfg(feature = "http-client")]
fn http_layer_protocol_policy(scheme: &str) -> HttpProtocolAllow {
    if let Some(policy) = http_layer_protocol_policy_from_config(scheme) {
        return policy;
    }
    match scheme {
        "http" | "https" | "git" | "ssh" => HttpProtocolAllow::Always,
        "ext" => HttpProtocolAllow::Never,
        // ftp/ftps and unknown schemes default to user-only.
        _ => HttpProtocolAllow::UserOnly,
    }
}

#[cfg(feature = "http-client")]
fn http_layer_protocol_policy_from_config(scheme: &str) -> Option<HttpProtocolAllow> {
    let context = sley_config::ConfigIncludeContext::new(None, None);
    let mut config = sley_config::load_pre_dispatch_config(None, &context).ok()?;
    // `None` folds in the process-global `-c`/`--config-env` fragment so
    // `git -c protocol.http.allow=user clone …` is visible here.
    if let Ok(parameters) = sley_config::injected_config_parameters(None) {
        let _ = sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            std::path::Path::new("."),
        );
    }
    if let Some(value) = config.get("protocol", Some(scheme), "allow") {
        return parse_http_protocol_allow(value);
    }
    if let Some(value) = config.get("protocol", None, "allow") {
        return parse_http_protocol_allow(value);
    }
    None
}

#[cfg(feature = "http-client")]
fn parse_http_protocol_allow(value: &str) -> Option<HttpProtocolAllow> {
    if value.eq_ignore_ascii_case("always") {
        Some(HttpProtocolAllow::Always)
    } else if value.eq_ignore_ascii_case("never") {
        Some(HttpProtocolAllow::Never)
    } else if value.eq_ignore_ascii_case("user") {
        Some(HttpProtocolAllow::UserOnly)
    } else {
        None
    }
}

/// Emit the stable request-side portion of Git's curl trace for Sley's native
/// HTTP client. The transfer remains in-process; this preserves the public
/// `GIT_TRACE_CURL` observability surface without manufacturing a helper
/// process. Response payloads and authorization values are intentionally not
/// logged.
#[cfg(feature = "http-client")]
fn trace_curl_request(method: &str, url: &str, headers: &[(&str, &str)], chunked: bool) {
    let Some(mut sink) = curl_trace_sink() else {
        return;
    };
    let rendered = curl_request_trace(method, url, headers, chunked);
    let _ = sink.write_all(rendered.as_bytes());
    let _ = sink.flush();
}

#[cfg(feature = "http-client")]
fn curl_request_trace(method: &str, url: &str, headers: &[(&str, &str)], chunked: bool) -> String {
    let display_url = sley_core::redact_url_for_display(url);
    let mut rendered = format!("=> Send header: {method} {display_url}\n");
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        rendered.push_str(&format!("=> Send header: {name}: {value}\n"));
    }
    if chunked {
        rendered.push_str("=> Send header: Transfer-Encoding: chunked\n");
    }
    rendered
}

#[cfg(feature = "http-client")]
fn curl_trace_sink() -> Option<Box<dyn Write>> {
    if !curl_trace_enabled() {
        return None;
    }
    // Destination resolution happens only on the enabled path; the enabled
    // probe itself is cached for the process lifetime (see `curl_trace_enabled`).
    let value = std::env::var("GIT_TRACE_CURL").ok()?;
    match value.to_ascii_lowercase().as_str() {
        "" | "0" | "false" => None,
        "1" | "2" | "true" => Some(Box::new(std::io::stderr())),
        _ if std::path::Path::new(&value).is_absolute() => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(value)
            .ok()
            .map(|file| Box::new(file) as Box<dyn Write>),
        _ => None,
    }
}

/// Whether `GIT_TRACE_CURL` names a usable trace destination.
///
/// Probed once and cached for the process lifetime (`OnceLock`) so the
/// per-request hot path does not re-read the environment; matches git's own
/// process-lifetime trace setup semantics. Mirrors `get_trace_fd`: disabled by
/// unset/empty/`0`/`false`, stderr for `1`/`2`/`true`, an append-mode file for
/// absolute paths, anything else unusable.
#[cfg(feature = "http-client")]
fn curl_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let Ok(value) = std::env::var("GIT_TRACE_CURL") else {
            return false;
        };
        match value.to_ascii_lowercase().as_str() {
            "" | "0" | "false" => false,
            "1" | "2" | "true" => true,
            _ => std::path::Path::new(&value).is_absolute(),
        }
    })
}

/// Map a genuine ureq transport/protocol failure to a [`GitError`], always
/// including the offending `url` in the message.
#[cfg(feature = "http-client")]
fn http_transport_error(url: &str, err: &ureq::Error) -> GitError {
    GitError::Io(format!("HTTP request to {url} failed: {err}"))
}

// Small private framing helpers, duplicated verbatim from git-protocol so the
// staying service-negotiation/credential code keeps working without exposing
// git-protocol's internal helpers as public API.
fn line(mut payload: Vec<u8>) -> Vec<u8> {
    payload.push(b'\n');
    payload
}

fn line_from_str(payload: &str) -> Vec<u8> {
    line(payload.as_bytes().to_vec())
}

fn trim_trailing_lf(input: &[u8]) -> &[u8] {
    input.strip_suffix(b"\n").unwrap_or(input)
}

fn validate_protocol_v2_line(label: &str, value: &[u8]) -> Result<()> {
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.iter().any(|byte| matches!(*byte, b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    Ok(())
}

fn parse_protocol_v2_line_text<'a>(label: &str, value: &'a [u8]) -> Result<&'a str> {
    validate_protocol_v2_line(label, value)?;
    let value = trim_trailing_lf(value);
    if value.is_empty() {
        return Err(GitError::InvalidFormat(format!("{label} is empty")));
    }
    if value.iter().any(|byte| matches!(*byte, b'\n' | b'\r' | 0)) {
        return Err(GitError::InvalidFormat(format!(
            "{label} contains a delimiter byte"
        )));
    }
    std::str::from_utf8(value).map_err(|err| GitError::InvalidFormat(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_core::{Capability, ObjectId};

    #[test]
    fn service_request_parses_and_encodes_host_and_protocol() {
        let payload = b"git-upload-pack /project.git\0host=example.com\0\0version=2\0agent=sley\0";
        let request = parse_service_request(payload).expect("test operation should succeed");
        assert_eq!(
            request,
            ServiceRequest {
                service: GitService::UploadPack,
                path: "/project.git".into(),
                host: Some("example.com".into()),
                parameters: Vec::new(),
                protocol: Some(ProtocolVersion::V2),
                extra_parameters: vec!["agent=sley".into()],
            }
        );
        assert_eq!(
            encode_service_request(&request).expect("test operation should succeed"),
            payload
        );
    }

    #[test]
    fn service_request_preserves_regular_and_extra_parameters() {
        let payload =
            b"git-receive-pack repo with spaces\0host=example.com\0version-hint=1\0\0trace=1\0";
        let request = parse_service_request(payload).expect("test operation should succeed");
        assert_eq!(
            request,
            ServiceRequest {
                service: GitService::ReceivePack,
                path: "repo with spaces".into(),
                host: Some("example.com".into()),
                parameters: vec!["version-hint=1".into()],
                protocol: None,
                extra_parameters: vec!["trace=1".into()],
            }
        );
        assert_eq!(
            encode_service_request(&request).expect("test operation should succeed"),
            payload
        );
    }

    #[test]
    fn service_request_streams_round_trip() {
        let request = ServiceRequest {
            service: GitService::UploadArchive,
            path: "/repo.git".into(),
            host: None,
            parameters: Vec::new(),
            protocol: Some(ProtocolVersion::V1),
            extra_parameters: Vec::new(),
        };
        let mut encoded = Vec::new();
        write_service_request(&mut encoded, &request).expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_service_request(&mut input).expect("test operation should succeed"),
            request
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn service_request_rejects_malformed_payloads() {
        assert!(parse_service_request(b"git-upload-pack").is_err());
        assert!(parse_service_request(b"git-not-a-service /repo.git").is_err());
        assert!(parse_service_request(b"git-upload-pack /repo.git\n").is_err());
        assert!(parse_service_request(b"git-upload-pack /repo.git\0host=example.com").is_err());
        assert!(parse_service_request(b"git-upload-pack /repo.git\0host=one\0host=two\0").is_err());
        assert!(
            parse_service_request(b"git-upload-pack /repo.git\0\0version=2\0version=1\0").is_err()
        );
        assert!(parse_service_request(b"git-upload-pack /repo.git\0\0version=0\0").is_err());
        assert!(
            encode_service_request(&ServiceRequest {
                service: GitService::UploadPack,
                path: "/repo.git".into(),
                host: None,
                parameters: Vec::new(),
                protocol: Some(ProtocolVersion::V0),
                extra_parameters: Vec::new(),
            })
            .is_err()
        );
        assert!(read_service_request(&mut &b"0000"[..]).is_err());
    }

    #[test]
    fn service_announcement_parses_and_encodes_stream() {
        let frames = vec![
            PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let announcement =
            parse_service_announcement_stream(&frames).expect("test operation should succeed");
        assert_eq!(
            announcement,
            ServiceAnnouncement {
                service: GitService::UploadPack,
            }
        );
        assert_eq!(
            encode_service_announcement(&announcement).expect("test operation should succeed"),
            b"# service=git-upload-pack\n"
        );
        assert_eq!(
            encode_service_announcement_stream(&announcement)
                .expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn service_announcement_streams_round_trip() {
        let announcement = ServiceAnnouncement {
            service: GitService::ReceivePack,
        };
        let mut encoded = Vec::new();
        write_service_announcement(&mut encoded, &announcement)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_service_announcement(&mut input).expect("test operation should succeed"),
            announcement
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn service_announcement_rejects_malformed_streams() {
        assert!(parse_service_announcement(b"service=git-upload-pack\n").is_err());
        assert!(parse_service_announcement(b"# service=git-not-a-service\n").is_err());
        assert!(parse_service_announcement(b"# service=git-upload-pack\0\n").is_err());
        assert!(parse_service_announcement_stream(&[]).is_err());
        assert!(
            parse_service_announcement_stream(&[PktLineFrame::Data(
                b"# service=git-upload-pack\n".to_vec(),
            )])
            .is_err()
        );
        assert!(
            parse_service_announcement_stream(&[
                PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
                PktLineFrame::Data(b"extra\n".to_vec()),
                PktLineFrame::Flush,
            ])
            .is_err()
        );
        assert!(parse_service_announcement_stream(&[PktLineFrame::Flush]).is_err());
        assert!(read_service_announcement(&mut &b"0000"[..]).is_err());
    }

    #[test]
    fn service_discovery_response_parses_v0_refs_after_announcement() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let frames = vec![
            PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
            PktLineFrame::Flush,
            PktLineFrame::Data(
                b"1111111111111111111111111111111111111111 HEAD\0multi_ack\n".to_vec(),
            ),
            PktLineFrame::Flush,
        ];
        let response = parse_service_discovery_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            response,
            ServiceDiscoveryResponse {
                announcement: ServiceAnnouncement {
                    service: GitService::UploadPack,
                },
                payload: ServiceDiscoveryPayload::AdvertisedRefs(RefAdvertisementSet {
                    protocol: ProtocolVersion::V0,
                    refs: vec![RefAdvertisement {
                        oid,
                        name: "HEAD".into(),
                        capabilities: vec![Capability {
                            name: "multi_ack".into(),
                            value: None,
                        }],
                    }],
                    shallow: Vec::new(),
                }),
            }
        );
        assert_eq!(
            encode_service_discovery_response(&response).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn service_discovery_response_parses_protocol_v2_after_announcement() {
        let frames = vec![
            PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
            PktLineFrame::Flush,
            PktLineFrame::Data(b"version 2\n".to_vec()),
            PktLineFrame::Data(b"ls-refs=unborn\n".to_vec()),
            PktLineFrame::Data(b"fetch=shallow filter\n".to_vec()),
            PktLineFrame::Flush,
        ];
        let response = parse_service_discovery_response(ObjectFormat::Sha1, &frames)
            .expect("test operation should succeed");
        assert_eq!(
            response,
            ServiceDiscoveryResponse {
                announcement: ServiceAnnouncement {
                    service: GitService::UploadPack,
                },
                payload: ServiceDiscoveryPayload::ProtocolV2(TransportHandshake {
                    protocol: ProtocolVersion::V2,
                    capabilities: vec![
                        Capability {
                            name: "ls-refs".into(),
                            value: Some("unborn".into()),
                        },
                        Capability {
                            name: "fetch".into(),
                            value: Some("shallow filter".into()),
                        },
                    ],
                }),
            }
        );
        assert_eq!(
            encode_service_discovery_response(&response).expect("test operation should succeed"),
            frames
        );
    }

    #[test]
    fn service_discovery_response_streams_round_trip() {
        let response = ServiceDiscoveryResponse {
            announcement: ServiceAnnouncement {
                service: GitService::ReceivePack,
            },
            payload: ServiceDiscoveryPayload::AdvertisedRefs(RefAdvertisementSet {
                protocol: ProtocolVersion::V1,
                refs: Vec::new(),
                shallow: Vec::new(),
            }),
        };
        let mut encoded = Vec::new();
        write_service_discovery_response(&mut encoded, &response)
            .expect("test operation should succeed");
        encoded.extend_from_slice(b"tail");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_service_discovery_response(ObjectFormat::Sha1, &mut input)
                .expect("test operation should succeed"),
            response
        );
        assert_eq!(input, b"tail");
    }

    #[test]
    fn service_discovery_response_rejects_malformed_streams() {
        assert!(parse_service_discovery_response(ObjectFormat::Sha1, &[]).is_err());
        assert!(
            parse_service_discovery_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_service_discovery_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
                    PktLineFrame::Data(b"version 2\n".to_vec()),
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_service_discovery_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
                    PktLineFrame::Flush,
                    PktLineFrame::Delimiter,
                    PktLineFrame::Flush,
                ],
            )
            .is_err()
        );
        assert!(
            parse_service_discovery_response(
                ObjectFormat::Sha1,
                &[
                    PktLineFrame::Data(b"# service=git-upload-pack\n".to_vec()),
                    PktLineFrame::Flush,
                    PktLineFrame::Data(b"version 2\n".to_vec()),
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn remote_url_parser_classifies_local_http_git_and_ssh_forms() {
        assert_eq!(
            parse_remote_url("../repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Local,
                user: None,
                password: None,
                host: None,
                port: None,
                path: "../repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("/srv/git/repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Local,
                user: None,
                password: None,
                host: None,
                port: None,
                path: "/srv/git/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("C:/work/repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Local,
                user: None,
                password: None,
                host: None,
                port: None,
                path: "C:/work/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("file:///tmp/repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::File,
                user: None,
                password: None,
                host: None,
                port: None,
                path: "/tmp/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("file:///Users/alice/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::File,
                user: None,
                password: None,
                host: None,
                port: None,
                path: "/Users/alice/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("https://example.com/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Https,
                user: None,
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("https://alice:s3cret@example.com/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Https,
                user: Some("alice".into()),
                password: Some("s3cret".into()),
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("https://token@example.com/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Https,
                user: Some("token".into()),
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("https://[2001:db8::2]:8443/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Https,
                user: None,
                password: None,
                host: Some("2001:db8::2".into()),
                port: Some(8443),
                path: "/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("http://example.com:8080/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Http,
                user: None,
                password: None,
                host: Some("example.com".into()),
                port: Some(8080),
                path: "/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("git://example.com/repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Git,
                user: None,
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("ssh://git@[2001:db8::1]:2222/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: Some("git".into()),
                password: None,
                host: Some("2001:db8::1".into()),
                port: Some(2222),
                path: "/org/repo.git".into(),
            }
        );
        // Deprecated scheme aliases parse as native SSH (t5813 git+ssh:// path).
        assert_eq!(
            parse_remote_url("git+ssh://git@example.com/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: Some("git".into()),
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("ssh+git://git@example.com/org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: Some("git".into()),
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("ssh://git@example.com/org/repo%20space/it%27s%2Fnested.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: Some("git".into()),
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "/org/repo space/it's/nested.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("git@example.com:org/repo.git")
                .expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: Some("git".into()),
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "org/repo.git".into(),
            }
        );
        assert_eq!(
            parse_remote_url("example.com:org/repo.git").expect("test operation should succeed"),
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: None,
                password: None,
                host: Some("example.com".into()),
                port: None,
                path: "org/repo.git".into(),
            }
        );
    }

    #[test]
    fn remote_url_parser_rejects_malformed_values() {
        assert!(parse_remote_url("").is_err());
        assert!(parse_remote_url("ftp://example.com/repo.git").is_err());
        assert!(parse_remote_url("https://example.com").is_err());
        assert!(parse_remote_url("https://exa mple/repo.git").is_err());
        assert!(parse_remote_url("ssh://host:abc/repo.git").is_err());
        assert!(parse_remote_url("ssh://[2001:db8::1/repo.git").is_err());
        assert!(parse_remote_url("ssh://host/repo%2").is_err());
        assert!(parse_remote_url("ssh://host/repo%0a.git").is_err());
        assert!(parse_remote_url("git@example.com:").is_err());
        assert!(parse_remote_url("repo.git\n").is_err());
    }

    #[test]
    fn git_credentials_parse_encode_and_preserve_extension_fields() {
        let credential = parse_git_credential(
            b"protocol=https\nhost=example.com\npath=org/repo.git\nusername=alice\npassword=secret\npassword_expiry_utc=1700000000\noauth_refresh_token=refresh\nurl=https://example.com/org/repo.git\nwwwauth[]=Bearer realm=one\nwwwauth[]=Basic realm=two\nquit=true\nhelper-state=opaque\n\n",
        )
        .expect("test operation should succeed");
        assert_eq!(
            credential,
            GitCredential {
                protocol: Some("https".into()),
                host: Some("example.com".into()),
                path: Some("org/repo.git".into()),
                username: Some("alice".into()),
                password: Some("secret".into()),
                password_expiry_utc: 1_700_000_000,
                oauth_refresh_token: Some("refresh".into()),
                url: Some("https://example.com/org/repo.git".into()),
                wwwauth: vec!["Bearer realm=one".into(), "Basic realm=two".into()],
                quit: true,
                extra: vec![("helper-state".into(), "opaque".into())],
                ..GitCredential::default()
            }
        );
        assert_eq!(
            encode_git_credential(&credential).expect("test operation should succeed"),
            b"protocol=https\nhost=example.com\npath=org/repo.git\nusername=alice\npassword=secret\npassword_expiry_utc=1700000000\noauth_refresh_token=refresh\nurl=https://example.com/org/repo.git\nwwwauth[]=Bearer realm=one\nwwwauth[]=Basic realm=two\nquit=true\nhelper-state=opaque\n\n"
        );
    }

    #[test]
    fn git_credentials_stream_round_trip() {
        let credential = GitCredential {
            protocol: Some("https".into()),
            host: Some("example.com".into()),
            username: Some("alice".into()),
            password: Some("secret".into()),
            ..GitCredential::default()
        };
        let mut encoded = Vec::new();
        write_git_credential(&mut encoded, &credential).expect("test operation should succeed");

        let mut input = encoded.as_slice();
        assert_eq!(
            read_git_credential(&mut input).expect("test operation should succeed"),
            credential
        );
    }

    #[test]
    fn git_credentials_reject_oversized_helper_responses() {
        let oversized = vec![b'x'; MAX_GIT_CREDENTIAL_RESPONSE_BYTES + 1];
        let mut input = oversized.as_slice();
        let err = read_git_credential(&mut input).expect_err("oversized response should fail");
        assert!(
            matches!(
                err,
                GitError::InvalidFormat(ref message)
                    if message.contains("credential helper response exceeds maximum size")
                        && message.contains("65536")
                        && message.contains("64 KiB")
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn git_credentials_accept_responses_at_size_limit() {
        let prefix = b"protocol=https\npadding=";
        let suffix = b"\n\n";
        let padding_len = MAX_GIT_CREDENTIAL_RESPONSE_BYTES - prefix.len() - suffix.len();
        let mut input = Vec::with_capacity(MAX_GIT_CREDENTIAL_RESPONSE_BYTES);
        input.extend_from_slice(prefix);
        input.extend(std::iter::repeat_n(b'x', padding_len));
        input.extend_from_slice(suffix);
        assert_eq!(input.len(), MAX_GIT_CREDENTIAL_RESPONSE_BYTES);

        let mut reader = input.as_slice();
        let credential =
            read_git_credential(&mut reader).expect("at-limit response should succeed");
        assert_eq!(credential.protocol.as_deref(), Some("https"));
        assert_eq!(
            credential.extra,
            vec![("padding".into(), "x".repeat(padding_len))]
        );
    }

    #[test]
    fn git_credentials_build_http_authorization_values() {
        let credential = GitCredential {
            username: Some("alice".into()),
            password: Some("secret".into()),
            ..GitCredential::default()
        };
        assert_eq!(
            git_credential_basic_authorization(&credential).expect("test operation should succeed"),
            Some("Basic YWxpY2U6c2VjcmV0".into())
        );
        assert_eq!(
            git_credential_basic_authorization(&GitCredential {
                username: Some("alice".into()),
                ..GitCredential::default()
            })
            .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            git_credential_bearer_authorization("token-123")
                .expect("test operation should succeed"),
            "Bearer token-123"
        );
    }

    #[test]
    fn git_credentials_reject_malformed_records() {
        assert!(parse_git_credential(b"protocol\n\n").is_err());
        assert!(parse_git_credential(b"=https\n\n").is_err());
        assert!(parse_git_credential(b"pro\0tocol=https\n\n").is_err());
        assert!(parse_git_credential(b"protocol=ht\0tps\n\n").is_err());
        assert!(parse_git_credential(b"protocol=http\rlive\n\n").is_err());
        assert!(parse_git_credential(b"protocol=http\nhost=example.com\n\ntrailing").is_err());
        assert!(parse_git_credential(b"protocol=http\nprotocol=https\n\n").is_err());
        assert!(
            encode_git_credential(&GitCredential {
                protocol: Some("http\nhttps".into()),
                ..GitCredential::default()
            })
            .is_err()
        );
        assert!(
            encode_git_credential(&GitCredential {
                extra: vec![("protocol".into(), "https".into())],
                ..GitCredential::default()
            })
            .is_err()
        );
        assert!(
            git_credential_basic_authorization(&GitCredential {
                username: Some("ali:ce".into()),
                password: Some("secret".into()),
                ..GitCredential::default()
            })
            .is_err()
        );
        assert!(
            git_credential_basic_authorization(&GitCredential {
                username: Some("alice".into()),
                password: Some("sec\nret".into()),
                ..GitCredential::default()
            })
            .is_err()
        );
        assert!(git_credential_bearer_authorization("").is_err());
        assert!(git_credential_bearer_authorization("tok\nen").is_err());
    }

    #[test]
    fn http_smart_urls_build_absolute_urls_from_remote() {
        // https without an explicit port.
        let remote = parse_remote_url("https://example.com/org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(
            http_smart_info_refs_url(&remote, GitService::UploadPack)
                .expect("test operation should succeed"),
            "https://example.com/org/repo.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(
            http_smart_rpc_url(&remote, GitService::UploadPack)
                .expect("test operation should succeed"),
            "https://example.com/org/repo.git/git-upload-pack"
        );

        // https with an explicit port and a .git suffix path.
        let remote = parse_remote_url("https://example.com:8443/org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(
            http_smart_info_refs_url(&remote, GitService::ReceivePack)
                .expect("test operation should succeed"),
            "https://example.com:8443/org/repo.git/info/refs?service=git-receive-pack"
        );
        assert_eq!(
            http_smart_rpc_url(&remote, GitService::ReceivePack)
                .expect("test operation should succeed"),
            "https://example.com:8443/org/repo.git/git-receive-pack"
        );

        // http scheme is honored too.
        let remote =
            parse_remote_url("http://example.com/repo").expect("test operation should succeed");
        assert_eq!(
            http_smart_info_refs_url(&remote, GitService::UploadPack)
                .expect("test operation should succeed"),
            "http://example.com/repo/info/refs?service=git-upload-pack"
        );
    }

    #[test]
    fn http_smart_urls_never_include_userinfo() {
        let remote = parse_remote_url("https://alice:s3cret@example.com/org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("alice"));
        assert_eq!(remote.password.as_deref(), Some("s3cret"));
        let info = http_smart_info_refs_url(&remote, GitService::UploadPack)
            .expect("test operation should succeed");
        let rpc = http_smart_rpc_url(&remote, GitService::UploadPack)
            .expect("test operation should succeed");
        assert_eq!(
            info,
            "https://example.com/org/repo.git/info/refs?service=git-upload-pack"
        );
        assert_eq!(rpc, "https://example.com/org/repo.git/git-upload-pack");
        assert!(!info.contains("alice"));
        assert!(!info.contains("s3cret"));
        assert!(!rpc.contains('@'));
    }

    #[test]
    fn http_smart_urls_bracket_ipv6_hosts() {
        let remote = parse_remote_url("https://[2001:db8::1]:8443/repo.git")
            .expect("test operation should succeed");
        assert_eq!(
            http_smart_rpc_url(&remote, GitService::UploadPack)
                .expect("test operation should succeed"),
            "https://[2001:db8::1]:8443/repo.git/git-upload-pack"
        );
    }

    #[test]
    fn http_smart_urls_reject_non_http_transports() {
        let remote = parse_remote_url("ssh://git@example.com/repo.git")
            .expect("test operation should succeed");
        assert!(http_smart_info_refs_url(&remote, GitService::UploadPack).is_err());
        assert!(http_smart_rpc_url(&remote, GitService::UploadPack).is_err());

        let remote =
            parse_remote_url("git://example.com/repo.git").expect("test operation should succeed");
        assert!(http_smart_info_refs_url(&remote, GitService::UploadPack).is_err());
    }

    #[test]
    fn remote_url_parser_extracts_http_password_but_not_ssh() {
        // http(s) userinfo is split into user + password.
        let remote = parse_remote_url("https://alice:s3cret@example.com/org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("alice"));
        assert_eq!(remote.password.as_deref(), Some("s3cret"));

        let remote = parse_remote_url("http://bob:pw@example.com:8080/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("bob"));
        assert_eq!(remote.password.as_deref(), Some("pw"));
        assert_eq!(remote.port, Some(8080));

        // http(s) user without a password leaves the password unset.
        let remote = parse_remote_url("https://token@example.com/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("token"));
        assert_eq!(remote.password, None);

        // SSH userinfo is preserved verbatim (no embedded-password concept), so
        // behavior does not regress for scp-like or ssh:// forms.
        let remote = parse_remote_url("ssh://git@example.com/org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("git"));
        assert_eq!(remote.password, None);

        let remote = parse_remote_url("git@example.com:org/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("git"));
        assert_eq!(remote.password, None);
    }

    #[test]
    fn ssh_service_command_builds_shell_quoted_commands() {
        assert_eq!(
            ssh_service_command(GitService::UploadPack, "/srv/repo.git")
                .expect("test operation should succeed"),
            "git-upload-pack '/srv/repo.git'"
        );
        assert_eq!(
            ssh_service_command(GitService::ReceivePack, "team/project.git")
                .expect("test operation should succeed"),
            "git-receive-pack 'team/project.git'"
        );
        assert_eq!(
            ssh_service_command(GitService::UploadArchive, "/srv/it's.git")
                .expect("test operation should succeed"),
            "git-upload-archive '/srv/it'\\''s.git'"
        );
        assert_eq!(
            ssh_service_command_with_program("./tools/git-upload-pack", "/srv/repo.git")
                .expect("explicit service command"),
            "./tools/git-upload-pack '/srv/repo.git'"
        );
        assert!(ssh_service_command_with_program("bad\ncommand", "/srv/repo.git").is_err());
    }

    #[test]
    fn ssh_process_args_honor_explicit_service_program() {
        let remote = parse_remote_url("localhost:/path/to/repo").expect("ssh remote");
        assert_eq!(
            ssh_process_args_with_ip_and_command(
                &remote,
                GitService::UploadPack,
                SshCommandVariant::Simple,
                None,
                Some("./something/bin/git-upload-pack"),
            )
            .expect("ssh args"),
            vec![
                "localhost".to_string(),
                "./something/bin/git-upload-pack '/path/to/repo'".to_string(),
            ]
        );
    }

    #[test]
    fn ssh_process_command_builds_openssh_arguments() {
        let remote = parse_remote_url("ssh://git@example.com:2222/srv/repo.git")
            .expect("test operation should succeed");
        assert_eq!(
            ssh_process_command(
                &remote,
                GitService::UploadPack,
                "ssh",
                SshCommandVariant::OpenSsh,
            )
            .expect("test operation should succeed"),
            SshProcessCommand {
                program: "ssh".into(),
                args: vec![
                    "-p".into(),
                    "2222".into(),
                    "git@example.com".into(),
                    "git-upload-pack '/srv/repo.git'".into(),
                ],
            }
        );

        let remote = parse_remote_url("ssh://git@[2001:db8::1]:2222/org/it%27s.git")
            .expect("test operation should succeed");
        assert_eq!(
            ssh_process_args(&remote, GitService::UploadPack, SshCommandVariant::OpenSsh)
                .expect("test operation should succeed"),
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "git@2001:db8::1".to_string(),
                "git-upload-pack '/org/it'\\''s.git'".to_string(),
            ]
        );

        let remote = parse_remote_url("ssh://user:passw@rd@example.com:2222/repo.git")
            .expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("user:passw@rd"));
        assert_eq!(remote.host.as_deref(), Some("example.com"));
        assert_eq!(
            ssh_process_args(&remote, GitService::UploadPack, SshCommandVariant::OpenSsh)
                .expect("test operation should succeed"),
            vec![
                "-p".to_string(),
                "2222".to_string(),
                "user:passw@rd@example.com".to_string(),
                "git-upload-pack '/repo.git'".to_string(),
            ]
        );
    }

    #[test]
    fn ssh_process_command_builds_scp_like_and_plink_arguments() {
        let remote = parse_remote_url("git@example.com:team/it isn't.git")
            .expect("test operation should succeed");
        assert_eq!(
            ssh_process_args(&remote, GitService::ReceivePack, SshCommandVariant::OpenSsh)
                .expect("test operation should succeed"),
            vec![
                "git@example.com".to_string(),
                "git-receive-pack 'team/it isn'\\''t.git'".to_string(),
            ]
        );

        let remote = parse_remote_url("example.com:team/project.git")
            .expect("test operation should succeed");
        assert_eq!(
            ssh_process_args(&remote, GitService::UploadPack, SshCommandVariant::OpenSsh)
                .expect("test operation should succeed"),
            vec![
                "example.com".to_string(),
                "git-upload-pack 'team/project.git'".to_string(),
            ]
        );

        let remote = parse_remote_url("[myhost:123]:src").expect("test operation should succeed");
        assert_eq!(
            remote,
            RemoteUrl {
                transport: RemoteTransport::Ssh,
                user: None,
                password: None,
                host: Some("myhost".into()),
                port: Some(123),
                path: "src".into(),
            }
        );
        assert_eq!(
            ssh_process_args(&remote, GitService::UploadPack, SshCommandVariant::OpenSsh)
                .expect("test operation should succeed"),
            vec![
                "-p".to_string(),
                "123".to_string(),
                "myhost".to_string(),
                "git-upload-pack 'src'".to_string(),
            ]
        );

        let remote = parse_remote_url("[::1]:rep").expect("test operation should succeed");
        assert_eq!(remote.host.as_deref(), Some("::1"));
        assert_eq!(remote.port, None);
        assert_eq!(remote.path, "rep");

        let remote = parse_remote_url("[user@::1]:repo").expect("test operation should succeed");
        assert_eq!(remote.user.as_deref(), Some("user"));
        assert_eq!(remote.host.as_deref(), Some("::1"));

        let remote = parse_remote_url("c:temp").expect("test operation should succeed");
        if cfg!(windows) {
            assert_eq!(remote.transport, RemoteTransport::Local);
        } else {
            assert_eq!(remote.transport, RemoteTransport::Ssh);
            assert_eq!(remote.host.as_deref(), Some("c"));
            assert_eq!(remote.path, "temp");
        }

        let remote = parse_remote_url("ssh://example.com:29418/team/project.git")
            .expect("test operation should succeed");
        assert_eq!(
            ssh_process_args(&remote, GitService::UploadArchive, SshCommandVariant::Plink,)
                .expect("test operation should succeed"),
            vec![
                "-P".to_string(),
                "29418".to_string(),
                "example.com".to_string(),
                "git-upload-archive '/team/project.git'".to_string(),
            ]
        );
        assert_eq!(
            ssh_process_args(
                &remote,
                GitService::UploadArchive,
                SshCommandVariant::TortoisePlink,
            )
            .expect("test operation should succeed"),
            vec![
                "-batch".to_string(),
                "-P".to_string(),
                "29418".to_string(),
                "example.com".to_string(),
                "git-upload-archive '/team/project.git'".to_string(),
            ]
        );
    }

    #[test]
    fn ssh_process_args_include_ip_family_by_variant() {
        let remote = parse_remote_url("[myhost:123]:src").expect("test operation should succeed");
        assert_eq!(
            ssh_process_args_with_ip(
                &remote,
                GitService::UploadPack,
                SshCommandVariant::OpenSsh,
                Some(SshIpVersion::V4),
            )
            .expect("test operation should succeed"),
            vec![
                "-4".to_string(),
                "-p".to_string(),
                "123".to_string(),
                "myhost".to_string(),
                "git-upload-pack 'src'".to_string(),
            ]
        );
        assert_eq!(
            ssh_process_args_with_ip(
                &remote,
                GitService::UploadPack,
                SshCommandVariant::Plink,
                Some(SshIpVersion::V6),
            )
            .expect("test operation should succeed"),
            vec![
                "-6".to_string(),
                "-P".to_string(),
                "123".to_string(),
                "myhost".to_string(),
                "git-upload-pack 'src'".to_string(),
            ]
        );
        assert!(
            ssh_process_args_with_ip(
                &remote,
                GitService::UploadPack,
                SshCommandVariant::Simple,
                Some(SshIpVersion::V4),
            )
            .is_err()
        );
    }

    #[test]
    fn ssh_scheme_accepts_git_ipv6_authority_forms() {
        for (url, user, host, port, path) in [
            (
                "ssh://::1/home/user/repo",
                None,
                "::1",
                None,
                "/home/user/repo",
            ),
            ("ssh://user@::1/~repo", Some("user"), "::1", None, "/~repo"),
            (
                "ssh://[user@::1]:22/home/user/repo",
                Some("user"),
                "::1",
                Some(22),
                "/home/user/repo",
            ),
            (
                "ssh://user@[::1]:/~repo",
                Some("user"),
                "::1",
                None,
                "/~repo",
            ),
        ] {
            let remote = parse_remote_url(url).expect("test operation should succeed");
            assert_eq!(remote.transport, RemoteTransport::Ssh);
            assert_eq!(remote.user.as_deref(), user);
            assert_eq!(remote.host.as_deref(), Some(host));
            assert_eq!(remote.port, port);
            assert_eq!(remote.path, path);
        }
    }

    #[test]
    fn ssh_process_command_rejects_invalid_inputs() {
        let local = parse_remote_url("../repo.git").expect("test operation should succeed");
        assert!(
            ssh_process_args(&local, GitService::UploadPack, SshCommandVariant::OpenSsh).is_err()
        );

        let remote = parse_remote_url("ssh://example.com:2222/repo.git")
            .expect("test operation should succeed");
        assert!(
            ssh_process_args(&remote, GitService::UploadPack, SshCommandVariant::Simple).is_err()
        );
        assert!(
            ssh_process_command(
                &remote,
                GitService::UploadPack,
                "ssh\n",
                SshCommandVariant::OpenSsh,
            )
            .is_err()
        );
    }

    #[test]
    fn ssh_service_command_rejects_delimited_paths() {
        assert!(ssh_service_command(GitService::UploadPack, "").is_err());
        assert!(ssh_service_command(GitService::UploadPack, "/repo.git\n").is_err());
        assert!(ssh_service_command(GitService::UploadPack, "/repo.git\r").is_err());
        assert!(ssh_service_command(GitService::UploadPack, "/repo.git\0").is_err());
    }

    #[test]
    fn git_protocol_header_parses_and_encodes_extra_parameters() {
        let header = GitProtocolHeader {
            protocol: Some(ProtocolVersion::V2),
            extra_parameters: vec!["agent=sley".into(), "trace".into()],
        };
        assert_eq!(
            encode_git_protocol_header(&header)
                .expect("test operation should succeed")
                .as_deref(),
            Some("version=2:agent=sley:trace")
        );
        assert_eq!(
            parse_git_protocol_header("version=2:agent=sley:trace")
                .expect("test operation should succeed"),
            header
        );
        assert_eq!(
            encode_git_protocol_header(&GitProtocolHeader::default())
                .expect("test operation should succeed"),
            None
        );
        assert_eq!(
            parse_git_protocol_header("agent=sley").expect("test operation should succeed"),
            GitProtocolHeader {
                protocol: None,
                extra_parameters: vec!["agent=sley".into()],
            }
        );
    }

    #[test]
    fn git_protocol_header_rejects_malformed_values() {
        assert!(parse_git_protocol_header("").is_err());
        assert!(parse_git_protocol_header("version=2:version=1").is_err());
        assert!(parse_git_protocol_header("version=0").is_err());
        assert!(parse_git_protocol_header("version=2:").is_err());
        assert!(parse_git_protocol_header("bad\nparameter").is_err());
        assert!(
            encode_git_protocol_header(&GitProtocolHeader {
                protocol: Some(ProtocolVersion::V0),
                extra_parameters: Vec::new(),
            })
            .is_err()
        );
        assert!(
            encode_git_protocol_header(&GitProtocolHeader {
                protocol: None,
                extra_parameters: vec!["bad:parameter".into()],
            })
            .is_err()
        );
    }

    #[cfg(feature = "http-client")]
    #[test]
    fn native_http_trace_reports_protocol_and_chunking_without_credentials() {
        let trace = curl_request_trace(
            "POST",
            "https://user:secret@example.test/repo/git-upload-pack",
            &[
                ("Git-Protocol", "version=2"),
                ("Authorization", "Basic c2VjcmV0"),
            ],
            true,
        );
        assert!(trace.contains("Send header: POST https://<redacted>@example.test/"));
        assert!(trace.contains("Send header: Git-Protocol: version=2"));
        assert!(trace.contains("Send header: Transfer-Encoding: chunked"));
        assert!(!trace.contains("Authorization"));
        assert!(!trace.contains("secret"));
    }

    #[cfg(feature = "tls-rustls")]
    #[test]
    fn extra_ca_bundle_requires_a_certificate() {
        let error = UreqHttpClient::with_extra_ca_certificate_pem(b"not a certificate")
            .err()
            .expect("a bundle without certificates must be rejected");
        assert!(
            error
                .to_string()
                .contains("TLS CA certificate bundle contains no certificates")
        );
    }
}

#[cfg(all(test, feature = "http-client"))]
mod http_timeout_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    #[test]
    fn packfile_get_uses_parallel_ranges_and_reassembles_bytes() {
        let body = Arc::new(
            (0..(5 * 1024 * 1024 + 37))
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind range server");
        let address = listener.local_addr().expect("range server address");
        let ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let server_body = Arc::clone(&body);
        let server_ranges = Arc::clone(&ranges);
        let server = std::thread::spawn(move || {
            let mut workers = Vec::new();
            for _ in 0..9 {
                let (mut stream, _) = listener.accept().expect("accept range request");
                let body = Arc::clone(&server_body);
                let ranges = Arc::clone(&server_ranges);
                workers.push(std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let count = stream.read(&mut buffer).expect("read range request");
                        assert!(count != 0, "range request ended before headers");
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let request = String::from_utf8(request).expect("HTTP request text");
                    let range = request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("range")
                                .then(|| value.trim().strip_prefix("bytes="))
                                .flatten()
                        })
                        .expect("Range header");
                    let (start, end) = range.split_once('-').expect("byte range");
                    let start = start.parse::<usize>().expect("range start");
                    let end = end.parse::<usize>().expect("range end");
                    ranges.lock().expect("ranges").push((start, end));
                    let response_body = &body[start..=end];
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                        response_body.len(),
                        body.len()
                    )
                    .expect("write range headers");
                    stream
                        .write_all(response_body)
                        .expect("write range response");
                }));
            }
            for worker in workers {
                worker.join().expect("range worker");
            }
        });

        let client = UreqHttpClient::new();
        let mut response = client
            .get_packfile(&format!("http://{address}/pack"))
            .expect("parallel packfile GET");
        let mut actual = Vec::new();
        response
            .body
            .read_to_end(&mut actual)
            .expect("read reassembled body");
        server.join().expect("range server");

        assert_eq!(actual, *body);
        let observed = ranges.lock().expect("ranges");
        assert_eq!(observed.len(), 9);
        assert!(observed.contains(&(0, 0)), "missing range probe");
        assert!(observed.iter().any(|(start, _)| *start > 0));
    }

    /// sley#163: ureq 3.3.0's `Timeouts::default()` leaves every field `None`
    /// except `await_100`, and an unset field is an unbounded phase. Name them
    /// all, so a field added to ureq later cannot silently reintroduce one.
    #[test]
    fn every_ureq_timeout_field_is_set() {
        let timeouts = UreqHttpClient::new().agent.config().timeouts();
        for (name, value) in [
            ("global", timeouts.global),
            ("per_call", timeouts.per_call),
            ("resolve", timeouts.resolve),
            ("connect", timeouts.connect),
            ("send_request", timeouts.send_request),
            ("await_100", timeouts.await_100),
            ("send_body", timeouts.send_body),
            ("recv_response", timeouts.recv_response),
            ("recv_body", timeouts.recv_body),
        ] {
            assert!(value.is_some(), "timeout `{name}` is unbounded");
        }
    }

    /// The body deadline is derived from the size ceiling and the floor rate,
    /// and the global deadline from the sum of the phases, so no part of the
    /// budget can be changed without the rest following.
    #[test]
    fn deadlines_are_derived_from_the_size_ceiling() {
        let limits = TransportLimits::default();
        assert_eq!(
            http_body_timeout(limits).as_secs(),
            MAX_PACKFILE_RESPONSE_BYTES / MIN_TRANSFER_BYTES_PER_SEC
        );
        assert_eq!(
            http_global_timeout(limits).as_secs(),
            HTTP_RESOLVE_TIMEOUT.as_secs()
                + HTTP_CONNECT_TIMEOUT.as_secs()
                + HTTP_SEND_REQUEST_TIMEOUT.as_secs()
                + HTTP_AWAIT_100_TIMEOUT.as_secs()
                + 2 * http_body_timeout(limits).as_secs()
        );
    }

    /// A default client is byte-for-byte what it was before the ceilings
    /// became configurable: configuring nothing changes nothing.
    #[test]
    fn an_unconfigured_client_keeps_the_documented_defaults() {
        let timeouts = UreqHttpClient::new().agent.config().timeouts();
        assert_eq!(
            timeouts.recv_body,
            Some(Duration::from_secs(
                MAX_PACKFILE_RESPONSE_BYTES / MIN_TRANSFER_BYTES_PER_SEC
            ))
        );
        assert_eq!(timeouts.send_body, timeouts.recv_body);
        assert_eq!(timeouts.recv_response, timeouts.recv_body);
        assert_eq!(UreqHttpClient::new().limits(), TransportLimits::default());
    }

    /// Raising the size ceiling raises the deadline that pays for it, in the
    /// same value -- the derivation is the point, not a coincidence of the
    /// default numbers.
    #[test]
    fn a_raised_ceiling_moves_the_body_deadline_with_it() {
        let raised = TransportLimits {
            max_packfile_response_bytes: 8 * 1024 * 1024 * 1024,
            ..TransportLimits::default()
        };
        let client = UreqHttpClient::with_limits(raised);
        let timeouts = client.agent.config().timeouts();
        assert_eq!(
            timeouts.recv_body,
            Some(Duration::from_secs(
                8 * 1024 * 1024 * 1024 / MIN_TRANSFER_BYTES_PER_SEC
            ))
        );
        assert!(timeouts.recv_body > Some(http_body_timeout(TransportLimits::default())));
        assert_eq!(timeouts.recv_response, timeouts.recv_body);
    }

    /// The ceiling moves; it does not disappear. Every field stays set and
    /// every deadline stays finite even when configuration asks for the
    /// largest ceiling and the slowest rate representable.
    #[test]
    fn no_configuration_can_build_a_client_without_deadlines() {
        let absurd = TransportLimits {
            max_ref_advertisement_bytes: u64::MAX,
            max_packfile_response_bytes: u64::MAX,
            min_transfer_bytes_per_sec: 1,
        };
        let client = UreqHttpClient::with_limits(absurd);
        assert!(
            client.limits().max_ref_advertisement_bytes
                <= sley_protocol::MAX_CONFIGURABLE_RESPONSE_BYTES
        );
        assert!(
            client.limits().max_packfile_response_bytes
                <= sley_protocol::MAX_CONFIGURABLE_RESPONSE_BYTES
        );
        let timeouts = client.agent.config().timeouts();
        for (name, value) in [
            ("global", timeouts.global),
            ("per_call", timeouts.per_call),
            ("resolve", timeouts.resolve),
            ("connect", timeouts.connect),
            ("send_request", timeouts.send_request),
            ("await_100", timeouts.await_100),
            ("send_body", timeouts.send_body),
            ("recv_response", timeouts.recv_response),
            ("recv_body", timeouts.recv_body),
        ] {
            let value = value.unwrap_or_else(|| panic!("timeout `{name}` is unbounded"));
            assert!(
                value <= sley_protocol::MAX_BODY_TRANSFER_TIMEOUT * 3,
                "timeout `{name}` is {value:?}, which is not a deadline"
            );
        }
        assert!(
            client.limits().body_transfer_timeout() <= sley_protocol::MAX_BODY_TRANSFER_TIMEOUT
        );
    }

    /// Zero reads as "unset", not as "refuse everything".
    #[test]
    fn a_zero_configured_ceiling_falls_back_to_the_default() {
        let zeroed = TransportLimits {
            max_ref_advertisement_bytes: 0,
            max_packfile_response_bytes: 0,
            min_transfer_bytes_per_sec: 0,
        };
        assert_eq!(zeroed.clamped(), TransportLimits::default());
    }

    /// A caller-supplied private CA must change the result of a real TLS
    /// handshake: the default Mozilla roots reject it, while the augmented
    /// client reaches the same HTTPS server successfully.
    #[cfg(feature = "tls-rustls")]
    #[test]
    fn private_ca_is_rejected_without_config_and_accepted_with_it() {
        use rcgen::{
            BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
            KeyPair, KeyUsagePurpose,
        };
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(
            ca_params,
            KeyPair::generate().expect("generate private CA key"),
        )
        .expect("generate private CA certificate");

        let mut server_params =
            CertificateParams::new(vec!["127.0.0.1".to_string()]).expect("server parameters");
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().expect("generate server key");
        let server_cert = server_params
            .signed_by(&server_key, &ca)
            .expect("sign server certificate with private CA");
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![server_cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
                )
                .expect("build TLS server configuration"),
        );

        let (default_url, default_server) = private_https_server(server_config.clone());
        let default_error = UreqHttpClient::new()
            .get(&default_url, &[])
            .err()
            .expect("the private CA must not be trusted without configuration");
        assert!(
            default_error.to_string().contains("UnknownIssuer"),
            "unexpected unconfigured-client error: {default_error}"
        );
        println!("unconfigured client rejected private CA: {default_error}");
        assert!(
            default_server
                .join()
                .expect("default server thread")
                .is_err(),
            "the unconfigured TLS handshake should be rejected"
        );

        let client = UreqHttpClient::with_extra_ca_certificate_pem(ca.pem().as_bytes())
            .expect("build client with private CA");
        match client.agent.config().tls_config().root_certs() {
            ureq::tls::RootCerts::Specific(certificates) => assert_eq!(
                certificates.len(),
                webpki_root_certs::TLS_SERVER_ROOT_CERTS.len() + 1,
                "the extra CA must augment, not replace, the bundled Mozilla roots"
            ),
            roots => panic!("configured client used unexpected roots: {roots:?}"),
        }
        let (configured_url, configured_server) = private_https_server(server_config);
        let mut response = client
            .get(&configured_url, &[])
            .expect("the configured private CA must be consulted");
        let mut body = String::new();
        response
            .body
            .read_to_string(&mut body)
            .expect("read private HTTPS response");
        assert_eq!(response.status, 200);
        assert_eq!(body, "private-ca-ok");
        configured_server
            .join()
            .expect("configured server thread")
            .expect("configured TLS request");
    }

    #[cfg(feature = "tls-rustls")]
    fn private_https_server(
        config: Arc<rustls::ServerConfig>,
    ) -> (String, std::thread::JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind private HTTPS server");
        let address = listener.local_addr().expect("private HTTPS address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            let connection = rustls::ServerConnection::new(config)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            let mut stream = rustls::StreamOwned::new(connection, stream);
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request)?;
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nprivate-ca-ok",
            )?;
            stream.flush()
        });
        (format!("https://{address}/"), server)
    }

    /// A peer that completes the TCP handshake and then says nothing must be
    /// abandoned on a deadline rather than held open forever.
    ///
    /// The request runs on a worker thread behind `recv_timeout`, so with no
    /// deadline configured this test fails on its own clock instead of hanging
    /// the suite.
    #[test]
    fn stalled_peer_fails_within_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let stall = std::thread::spawn(move || {
            // Accept, then never answer. Holding the stream keeps the
            // connection established rather than resetting it.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(120));
                drop(stream);
            }
        });

        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let client = UreqHttpClient::new();
            let started = Instant::now();
            let outcome = client.get(&format!("http://{addr}/info/refs"), &[]);
            let _ = tx.send((outcome.is_err(), started.elapsed()));
        });

        match rx.recv_timeout(Duration::from_secs(45)) {
            Ok((is_err, elapsed)) => {
                assert!(
                    is_err,
                    "a peer that never answers must not produce a response"
                );
                println!("stalled peer refused after {elapsed:?}");
            }
            Err(_) => panic!(
                "request to a stalled peer was still blocked after 45s: \
                 no read/response deadline fired"
            ),
        }

        drop(stall);
        drop(worker);
    }
}
