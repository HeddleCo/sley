// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_core::{GitError, ObjectFormat, Result};
use sley_protocol::*;
use std::io::{Read, Write};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProcessCommand {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitCredential {
    pub protocol: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_expiry_utc: Option<String>,
    pub oauth_refresh_token: Option<String>,
    pub url: Option<String>,
    pub wwwauth: Vec<String>,
    pub quit: Option<String>,
    pub extra: Vec<(String, String)>,
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
    if let Some((scheme, rest)) = value.split_once("://") {
        return parse_remote_url_with_scheme(scheme, rest);
    }
    if let Some(colon) = scp_like_separator(value) {
        let (authority, path) = value.split_at(colon);
        let path = &path[1..];
        validate_remote_path("remote path", path)?;
        let (user, _password, host, port) = parse_remote_authority(authority, false, false)?;
        if port.is_some() {
            return Err(GitError::InvalidFormat(
                "scp-like SSH remote must not include a port".into(),
            ));
        }
        return Ok(RemoteUrl {
            transport: RemoteTransport::Ssh,
            user,
            password: None,
            host: Some(host),
            port: None,
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
    let mut credential = GitCredential::default();
    let mut lines = input.split(|byte| *byte == b'\n');
    while let Some(raw_line) = lines.next() {
        if raw_line.is_empty() {
            if lines.any(|line| !line.is_empty()) {
                return Err(GitError::InvalidFormat(
                    "credential input contains data after terminator".into(),
                ));
            }
            break;
        }
        if raw_line.ends_with(b"\r") {
            return Err(GitError::InvalidFormat(
                "credential line contains a delimiter byte".into(),
            ));
        }
        let line = std::str::from_utf8(raw_line)
            .map_err(|_| GitError::InvalidFormat("credential line is not UTF-8".into()))?;
        let (key, value) = line.split_once('=').ok_or_else(|| {
            GitError::InvalidFormat("credential line is missing = delimiter".into())
        })?;
        validate_credential_key(key)?;
        validate_credential_value("credential value", value)?;
        match key {
            "protocol" => set_credential_field(&mut credential.protocol, key, value)?,
            "host" => set_credential_field(&mut credential.host, key, value)?,
            "path" => set_credential_field(&mut credential.path, key, value)?,
            "username" => set_credential_field(&mut credential.username, key, value)?,
            "password" => set_credential_field(&mut credential.password, key, value)?,
            "password_expiry_utc" => {
                set_credential_field(&mut credential.password_expiry_utc, key, value)?
            }
            "oauth_refresh_token" => {
                set_credential_field(&mut credential.oauth_refresh_token, key, value)?
            }
            "url" => set_credential_field(&mut credential.url, key, value)?,
            "wwwauth[]" => credential.wwwauth.push(value.to_string()),
            "quit" => set_credential_field(&mut credential.quit, key, value)?,
            _ => credential.extra.push((key.to_string(), value.to_string())),
        }
    }
    Ok(credential)
}

pub fn encode_git_credential(credential: &GitCredential) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_credential_field(&mut out, "protocol", credential.protocol.as_deref())?;
    encode_credential_field(&mut out, "host", credential.host.as_deref())?;
    encode_credential_field(&mut out, "path", credential.path.as_deref())?;
    encode_credential_field(&mut out, "username", credential.username.as_deref())?;
    encode_credential_field(&mut out, "password", credential.password.as_deref())?;
    encode_credential_field(
        &mut out,
        "password_expiry_utc",
        credential.password_expiry_utc.as_deref(),
    )?;
    encode_credential_field(
        &mut out,
        "oauth_refresh_token",
        credential.oauth_refresh_token.as_deref(),
    )?;
    encode_credential_field(&mut out, "url", credential.url.as_deref())?;
    for value in &credential.wwwauth {
        validate_credential_value("credential wwwauth[] value", value)?;
        out.extend_from_slice(b"wwwauth[]=");
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    encode_credential_field(&mut out, "quit", credential.quit.as_deref())?;
    for (key, value) in &credential.extra {
        validate_credential_key(key)?;
        if is_known_credential_key(key) {
            return Err(GitError::InvalidFormat(format!(
                "credential extra key duplicates known key {key}"
            )));
        }
        validate_credential_value("credential extra value", value)?;
        out.extend_from_slice(key.as_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    Ok(out)
}

pub fn read_git_credential(reader: &mut impl Read) -> Result<GitCredential> {
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    parse_git_credential(&input)
}

pub fn write_git_credential(writer: &mut impl Write, credential: &GitCredential) -> Result<()> {
    encode_credential_field(writer, "protocol", credential.protocol.as_deref())?;
    encode_credential_field(writer, "host", credential.host.as_deref())?;
    encode_credential_field(writer, "path", credential.path.as_deref())?;
    encode_credential_field(writer, "username", credential.username.as_deref())?;
    encode_credential_field(writer, "password", credential.password.as_deref())?;
    encode_credential_field(
        writer,
        "password_expiry_utc",
        credential.password_expiry_utc.as_deref(),
    )?;
    encode_credential_field(
        writer,
        "oauth_refresh_token",
        credential.oauth_refresh_token.as_deref(),
    )?;
    encode_credential_field(writer, "url", credential.url.as_deref())?;
    for value in &credential.wwwauth {
        validate_credential_value("credential wwwauth[] value", value)?;
        writer.write_all(b"wwwauth[]=")?;
        writer.write_all(value.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    encode_credential_field(writer, "quit", credential.quit.as_deref())?;
    for (key, value) in &credential.extra {
        validate_credential_key(key)?;
        if is_known_credential_key(key) {
            return Err(GitError::InvalidFormat(format!(
                "credential extra key duplicates known key {key}"
            )));
        }
        validate_credential_value("credential extra value", value)?;
        writer.write_all(key.as_bytes())?;
        writer.write_all(b"=")?;
        writer.write_all(value.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.write_all(b"\n")?;
    Ok(())
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
    validate_ssh_repository_path(repository_path)?;
    Ok(format!(
        "{} {}",
        service.as_str(),
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
    if remote.transport != RemoteTransport::Ssh {
        return Err(GitError::InvalidFormat(
            "SSH process arguments require an SSH remote".into(),
        ));
    }
    let mut args = Vec::new();
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
    args.push(ssh_service_command(service, &remote.path)?);
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
        "ssh" | "git" | "http" | "https" => {
            let is_http = scheme == "http" || scheme == "https";
            let (authority, path) = split_remote_authority_and_path(rest)?;
            // Only http(s) userinfo may carry an embedded password; SSH/git keep
            // their authority verbatim so existing behavior does not regress.
            let (user, password, host, port) = parse_remote_authority(authority, true, is_http)?;
            let path = if scheme == "ssh" {
                percent_decode_remote_path(&path)?
            } else {
                path
            };
            Ok(RemoteUrl {
                transport: match scheme.as_str() {
                    "ssh" => RemoteTransport::Ssh,
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
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            if idx + 2 >= bytes.len() {
                return Err(GitError::InvalidFormat(format!(
                    "invalid percent-encoded remote path {value:?}"
                )));
            }
            let high = percent_hex_value(bytes[idx + 1]).ok_or_else(|| {
                GitError::InvalidFormat(format!("invalid percent-encoded remote path {value:?}"))
            })?;
            let low = percent_hex_value(bytes[idx + 2]).ok_or_else(|| {
                GitError::InvalidFormat(format!("invalid percent-encoded remote path {value:?}"))
            })?;
            out.push((high << 4) | low);
            idx += 3;
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    let decoded = String::from_utf8(out).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    validate_remote_path("remote path", &decoded)?;
    Ok(decoded)
}

fn percent_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parsed remote authority: `(user, password, host, port)`. Password is only
/// ever populated for http(s) authorities (see `parse_remote_authority`).
type ParsedAuthority = (Option<String>, Option<String>, String, Option<u16>);

fn parse_remote_authority(
    value: &str,
    allow_port: bool,
    split_password: bool,
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
    let (host, port) = parse_remote_host_port(host_port, allow_port)?;
    validate_remote_host(&host)?;
    Ok((user, password, host, port))
}

fn parse_remote_host_port(value: &str, allow_port: bool) -> Result<(String, Option<u16>)> {
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
    let colon = value.find(':')?;
    if value[..colon].contains('/') {
        return None;
    }
    if colon == 1
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
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
    if value.bytes().any(|byte| {
        matches!(
            byte,
            b'@' | b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | 0
        )
    }) {
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
    pub body: Box<dyn std::io::Read + Send>,
}

/// Minimal byte-transport over HTTP(S) used to drive smart-HTTP git transport.
///
/// Implementations must surface HTTP error statuses (401/403/404/5xx) as
/// `Ok(HttpResponse { status, .. })` so callers can react to them (for example,
/// retrying a 401 with credentials). Only genuine transport failures
/// (DNS/connect/TLS/timeout/protocol) are reported as `Err`.
#[cfg(feature = "http-client")]
pub trait HttpClient {
    /// Issue a `GET` for `url`, sending the additional `headers`.
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse>;

    /// Issue a `POST` for `url` with `body`, sending `content_type` and the
    /// additional `headers`.
    fn post(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse>;
}

/// [`HttpClient`] backed by [`ureq`] with rustls + bundled Mozilla roots.
#[cfg(feature = "http-client")]
pub struct UreqHttpClient {
    agent: ureq::Agent,
}

#[cfg(feature = "http-client")]
impl UreqHttpClient {
    pub fn new() -> Self {
        // `http_status_as_error(false)` makes ureq deliver 4xx/5xx as a normal
        // response (carrying status + body) rather than an error, which is what
        // smart-HTTP callers need (e.g. inspecting 401 to prompt for creds).
        let mut builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(HTTP_USER_AGENT);
        if let Some(tls_config) = ureq_tls_config() {
            builder = builder.tls_config(tls_config);
        }
        Self {
            agent: builder.build().into(),
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
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request
            .call()
            .map_err(|err| http_transport_error(url, &err))?;
        Ok(http_response_from_ureq(response))
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HttpResponse> {
        let mut request = self.agent.post(url).header("Content-Type", content_type);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request
            .send(body)
            .map_err(|err| http_transport_error(url, &err))?;
        Ok(http_response_from_ureq(response))
    }
}

/// Convert a successful ureq response into an [`HttpResponse`] that streams its
/// body from the connection (the body is never buffered into memory here).
#[cfg(feature = "http-client")]
fn http_response_from_ureq(response: ureq::http::Response<ureq::Body>) -> HttpResponse {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(ureq::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let body = response.into_body().into_reader();
    HttpResponse {
        status,
        content_type,
        body: Box::new(body),
    }
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
                password_expiry_utc: Some("1700000000".into()),
                oauth_refresh_token: Some("refresh".into()),
                url: Some("https://example.com/org/repo.git".into()),
                wwwauth: vec!["Bearer realm=one".into(), "Basic realm=two".into()],
                quit: Some("true".into()),
                extra: vec![("helper-state".into(), "opaque".into())],
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
        assert!(parse_git_credential(b"protocol=http\r\n\n").is_err());
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
            .expect("test operation should succeed")[0],
            "-P"
        );
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
}
