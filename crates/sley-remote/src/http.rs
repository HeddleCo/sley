//! Shared smart-HTTP(S) transport plumbing.
//!
//! These are the reusable request/advertisement/auth helpers behind the HTTP
//! fetch/push/clone/ls-remote paths. They drive the transport-agnostic protocol
//! codecs ([`sley_protocol`]) over an [`HttpClient`] from [`sley_transport`],
//! taking everything as explicit parameters and never touching process-global
//! state, argument parsing, or stdout/stderr — so both the CLI orchestration and
//! an embedder can call them.
//!
//! HTTP mirrors the SSH path, with two wire differences: the info/refs GET
//! carries a `# service=` announcement preamble (handled by
//! [`read_service_discovery_response`]), and the RPC POST response goes straight
//! to the packfile/report (no re-advertised refs to skip).

use std::path::Path;

use sley_core::{
    Capability, GitError, ObjectFormat, ObjectId, Result, UPSTREAM_GIT_COMPAT_VERSION,
};
use sley_fetch::{
    install_protocol_v2_fetch_response_packfile,
    install_protocol_v2_fetch_response_promisor_packfile,
    install_upload_pack_raw_promisor_response_from_reader,
    install_upload_pack_raw_response_from_reader,
    install_upload_pack_shallow_raw_promisor_response_from_reader,
    install_upload_pack_shallow_raw_response_from_reader,
};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    GitService, ProtocolV2CommandOptions, ProtocolV2CommandRequest, ProtocolV2FetchRequest,
    ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo, ProtocolV2LsRefsRequest,
    RefAdvertisement, RefAdvertisementSet, TransportHandshake, UploadPackFeatures,
    UploadPackNegotiationRequest, UploadPackRawPackfileResponse, UploadPackRequest,
    encode_protocol_v2_command_options, parse_protocol_v2_fetch_features,
    parse_upload_pack_features, protocol_v2_object_format, read_protocol_v2_fetch_response,
    read_protocol_v2_fetch_sideband_all_response,
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set,
    read_upload_pack_raw_packfile_response,
    read_upload_pack_shallow_info_and_raw_packfile_response, smart_http_advertisement_content_type,
    smart_http_rpc_request_content_type, smart_http_rpc_result_content_type,
    validate_protocol_v2_fetch_command_request, validate_protocol_v2_ls_refs_command_request,
    write_protocol_v2_command_request, write_upload_pack_negotiation_request,
    write_upload_pack_request,
};
use sley_transport::{
    HttpClient, HttpResponse, RemoteTransport, RemoteUrl, ServiceDiscoveryPayload, UreqHttpClient,
    git_credential_basic_authorization, http_smart_info_refs_url, http_smart_rpc_url,
    parse_remote_url, read_service_discovery_response,
};

use crate::CredentialProvider;
use crate::credentials::{credential_request_for_url, http_url_credential};

/// Whether an already-resolved remote `url` uses HTTP(S) transport.
///
/// Callers that start from a configured remote name or relative source resolve
/// the URL first (the resolution is repository/process-state dependent and lives
/// in the caller); this only classifies a concrete URL.
pub fn remote_url_is_http(url: &str) -> Result<bool> {
    Ok(matches!(
        parse_remote_url(url)?.transport,
        RemoteTransport::Http | RemoteTransport::Https
    ))
}

/// Construct the default HTTP client used for smart-HTTP transport.
pub fn new_http_client() -> UreqHttpClient {
    UreqHttpClient::new()
}

/// Perform an HTTP request, retrying once with credential-provider-supplied
/// authentication if the first attempt returns 401. `perform` is invoked with an
/// optional `Authorization` header value and must be idempotent (it may run twice).
/// A successful retry approves the credential with `credentials`; a still-401
/// retry rejects it.
pub fn http_send_with_auth(
    remote: &RemoteUrl,
    credentials: &mut dyn CredentialProvider,
    mut perform: impl FnMut(Option<&str>) -> Result<HttpResponse>,
) -> Result<HttpResponse> {
    let initial = http_url_credential(remote);
    let initial_header = match &initial {
        Some(credential) => git_credential_basic_authorization(credential)?,
        None => None,
    };
    let response = perform(initial_header.as_deref())?;
    if response.status != 401 {
        return Ok(response);
    }
    let mut request = credential_request_for_url(remote);
    if request.username.is_none() {
        request.username = initial.and_then(|credential| credential.username);
    }
    let Some(filled) = credentials.fill(request)? else {
        return Ok(response);
    };
    let Some(header) = git_credential_basic_authorization(&filled)? else {
        return Ok(response);
    };
    let retry = perform(Some(&header))?;
    if retry.status != 401 {
        credentials.approve(&filled)?;
    } else {
        credentials.reject(&filled)?;
    }
    Ok(retry)
}

/// Build the `Authorization` header list for an optional credential header value.
pub fn http_authorization_headers(auth: Option<&str>) -> Vec<(&str, &str)> {
    match auth {
        Some(value) => vec![("Authorization", value)],
        None => Vec::new(),
    }
}

/// Map an HTTP response status to success or a descriptive error for `url`.
pub fn http_check_status(response: &HttpResponse, url: &str) -> Result<()> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else if response.status == 401 {
        Err(GitError::Command(format!(
            "authentication failed for {url}"
        )))
    } else {
        Err(GitError::Command(format!(
            "unexpected HTTP status {} for {url}",
            response.status
        )))
    }
}

/// Verify the response `Content-Type` matches `expected` (ignoring parameters).
pub fn http_validate_content_type(response: &HttpResponse, expected: &str) -> Result<()> {
    let actual = response
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(GitError::InvalidFormat(format!(
            "unexpected content type {actual:?}, expected {expected:?}"
        )))
    }
}

/// Result of smart-HTTP service discovery: parsed ref advertisements plus the
/// protocol v2 handshake when the server negotiated v2 on the info/refs exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpServiceAdvertisements {
    pub set: RefAdvertisementSet,
    pub handshake: Option<TransportHandshake>,
}

/// Parse a smart-HTTP info/refs body into a ref advertisement set for protocol
/// v0/v1. Protocol v2 discovery responses require a follow-up `ls-refs` RPC; use
/// [`http_service_advertisements`] instead.
pub fn http_advertised_refs(
    format: ObjectFormat,
    mut response: HttpResponse,
) -> Result<RefAdvertisementSet> {
    let discovery = read_service_discovery_response(format, &mut response.body)?;
    match discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(set),
        ServiceDiscoveryPayload::ProtocolV2(_) => Err(GitError::Unsupported(
            "protocol v2 advertisements over HTTP require an ls-refs RPC; use http_service_advertisements".into(),
        )),
    }
}

fn protocol_v2_ls_refs_command_request(
    format: ObjectFormat,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2CommandRequest> {
    let ls_refs = ProtocolV2LsRefsRequest {
        peel: true,
        symrefs: true,
        unborn: false,
        ref_prefixes: vec!["HEAD".into(), "refs/heads/".into(), "refs/tags/".into()],
    };
    let mut command = ls_refs.to_command_request()?;
    let mut options = ProtocolV2CommandOptions::default();
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "agent")
    {
        options.agent = Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}"));
    }
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "object-format")
    {
        let advertised_format = protocol_v2_object_format(&handshake.capabilities)?;
        if advertised_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                advertised_format.name(),
                format.name()
            )));
        }
        options.object_format = Some(format);
    }
    command.capabilities = encode_protocol_v2_command_options(&options)?;
    validate_protocol_v2_ls_refs_command_request(handshake, &command)?;
    Ok(command)
}

fn protocol_v2_fetch_command_request(
    format: ObjectFormat,
    handshake: &TransportHandshake,
    fetch: &ProtocolV2FetchRequest,
) -> Result<ProtocolV2CommandRequest> {
    let mut command = fetch.to_command_request()?;
    let mut options = ProtocolV2CommandOptions::default();
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "agent")
    {
        options.agent = Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}"));
    }
    if handshake
        .capabilities
        .iter()
        .any(|capability| capability.name == "object-format")
    {
        let advertised_format = protocol_v2_object_format(&handshake.capabilities)?;
        if advertised_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                advertised_format.name(),
                format.name()
            )));
        }
        options.object_format = Some(format);
    }
    command.capabilities = encode_protocol_v2_command_options(&options)?;
    validate_protocol_v2_fetch_command_request(handshake, format, &command)?;
    Ok(command)
}

fn protocol_v2_fetch_request_from_upload_pack_semantics(
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2FetchRequest> {
    let sideband_all = parse_protocol_v2_fetch_features(&handshake.capabilities)?
        .map(|features| features.sideband_all)
        .unwrap_or(false);
    Ok(ProtocolV2FetchRequest {
        wants,
        haves,
        shallow,
        deepen,
        done: true,
        sideband_all,
        ..ProtocolV2FetchRequest::default()
    })
}

fn shallow_info_from_protocol_v2_fetch_sections(
    sections: &[ProtocolV2FetchResponseSection],
) -> Vec<ProtocolV2FetchShallowInfo> {
    let mut shallow_info = Vec::new();
    for section in sections {
        if let ProtocolV2FetchResponseSection::ShallowInfo(entries) = section {
            shallow_info.extend(entries.clone());
        }
    }
    shallow_info
}

fn http_protocol_v2_ls_refs_advertisements(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    service: GitService,
    handshake: TransportHandshake,
    credentials: &mut dyn CredentialProvider,
) -> Result<RefAdvertisementSet> {
    let command = protocol_v2_ls_refs_command_request(format, &handshake)?;
    let url = http_smart_rpc_url(remote, service)?;
    let mut body = Vec::new();
    write_protocol_v2_command_request(&mut body, &command)?;
    let content_type = smart_http_rpc_request_content_type(service)?;
    let mut response = http_send_with_auth(remote, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &http_authorization_headers(auth),
            &body,
        )
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(&response, &smart_http_rpc_result_content_type(service)?)?;
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set(format, &mut response.body)
}

/// Fetch and parse the ref advertisements for `service` from the smart-HTTP
/// info/refs endpoint, authenticating and validating status + content type.
pub fn http_service_advertisements(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    service: GitService,
    credentials: &mut dyn CredentialProvider,
) -> Result<HttpServiceAdvertisements> {
    let url = http_smart_info_refs_url(remote, service)?;
    let mut response = http_send_with_auth(remote, credentials, |auth| {
        client.get(&url, &http_authorization_headers(auth))
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(&response, &smart_http_advertisement_content_type(service)?)?;
    let discovery = read_service_discovery_response(format, &mut response.body)?;
    match discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(HttpServiceAdvertisements {
            set,
            handshake: None,
        }),
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            let set = http_protocol_v2_ls_refs_advertisements(
                client,
                remote,
                format,
                service,
                handshake.clone(),
                credentials,
            )?;
            Ok(HttpServiceAdvertisements {
                set,
                handshake: Some(handshake),
            })
        }
    }
}

/// The upload-pack ref advertisements and parsed features for `remote`.
pub fn http_upload_pack_advertisements(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    credentials: &mut dyn CredentialProvider,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    let discovered =
        http_service_advertisements(client, remote, format, GitService::UploadPack, credentials)?;
    let features = upload_pack_features_from_advertisements(&discovered.set.refs)?;
    Ok((discovered.set.refs, features))
}

fn upload_pack_features_from_advertisements(
    advertisements: &[RefAdvertisement],
) -> Result<UploadPackFeatures> {
    Ok(advertisements
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default())
}

/// Post an upload-pack RPC `request` + `haves` and return the validated HTTP
/// response with its body still unread, so the caller can parse the packfile
/// stream (with or without a leading shallow-info section). Authenticates and
/// validates status + content type.
fn http_upload_pack_post(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    request: &UploadPackRequest,
    haves: Vec<ObjectId>,
    credentials: &mut dyn CredentialProvider,
) -> Result<HttpResponse> {
    let url = http_smart_rpc_url(remote, GitService::UploadPack)?;
    let mut body = Vec::new();
    write_upload_pack_request(&mut body, Some(request))?;
    write_upload_pack_negotiation_request(
        &mut body,
        &UploadPackNegotiationRequest { haves, done: true },
    )?;
    let content_type = smart_http_rpc_request_content_type(GitService::UploadPack)?;
    let response = http_send_with_auth(remote, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &http_authorization_headers(auth),
            &body,
        )
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(
        &response,
        &smart_http_rpc_result_content_type(GitService::UploadPack)?,
    )?;
    Ok(response)
}

/// Post an upload-pack RPC `request` + `haves` and read back the raw packfile
/// response, authenticating and validating status + content type. For a plain
/// (non-deepen) request; see [`http_upload_pack_shallow_fetch_response`] for the
/// deepen case where the response carries a leading shallow-info section.
pub fn http_upload_pack_fetch_response(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
    credentials: &mut dyn CredentialProvider,
) -> Result<UploadPackRawPackfileResponse> {
    let mut response = http_upload_pack_post(client, remote, &request, haves, credentials)?;
    read_upload_pack_raw_packfile_response(format, &mut response.body)
}

/// Post a deepen upload-pack RPC `request` + `haves` and read back the shallow-info
/// section plus the raw packfile response. Use this when `request` carries a
/// `shallow`/`deepen`/`deepen-since`/`deepen-not` argument: git always prefixes the
/// response with a shallow-info section (possibly empty) in that case. The returned
/// [`ProtocolV2FetchShallowInfo`] entries are the server's `shallow`/`unshallow`
/// updates for `$GIT_DIR/shallow`.
pub fn http_upload_pack_shallow_fetch_response(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
    credentials: &mut dyn CredentialProvider,
) -> Result<(
    Vec<ProtocolV2FetchShallowInfo>,
    UploadPackRawPackfileResponse,
)> {
    let mut response = http_upload_pack_post(client, remote, &request, haves, credentials)?;
    read_upload_pack_shallow_info_and_raw_packfile_response(format, &mut response.body)
}

/// Post a protocol v2 `fetch` RPC with `wants`/`haves`/`shallow`/`deepen` and
/// read back the sectioned response. Authenticates and validates status + content
/// type. When the server advertises `sideband-all`, the request and response use
/// the sideband-all wire form.
pub fn http_protocol_v2_fetch_response(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    handshake: &TransportHandshake,
    fetch: ProtocolV2FetchRequest,
    credentials: &mut dyn CredentialProvider,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let command = protocol_v2_fetch_command_request(format, handshake, &fetch)?;
    let url = http_smart_rpc_url(remote, GitService::UploadPack)?;
    let mut body = Vec::new();
    write_protocol_v2_command_request(&mut body, &command)?;
    let content_type = smart_http_rpc_request_content_type(GitService::UploadPack)?;
    let mut response = http_send_with_auth(remote, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &http_authorization_headers(auth),
            &body,
        )
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(
        &response,
        &smart_http_rpc_result_content_type(GitService::UploadPack)?,
    )?;
    if fetch.sideband_all {
        Ok(read_protocol_v2_fetch_sideband_all_response(format, &mut response.body)?.sections)
    } else {
        read_protocol_v2_fetch_response(format, &mut response.body)
    }
}

/// Fetch `wants` from an HTTP upload-pack remote into the repository at `git_dir`,
/// installing the resulting pack. Objects already present locally are skipped (for
/// non-shallow fetches); `promisor` selects promisor-pack installation.
///
/// When `deepen` is set the fetch is shallow: the request replays `shallow` (the
/// client's current boundary, read from `$GIT_DIR/shallow`) and asks the server to
/// truncate history to `deepen` commits. The returned [`ProtocolV2FetchShallowInfo`]
/// entries are the server's shallow-info updates the caller must fold into
/// `$GIT_DIR/shallow` (see [`crate::apply_shallow_info`]); they are empty for a
/// non-deepen fetch.
pub struct HttpFetchPackRequest<'a> {
    /// HTTP client used for smart-HTTP RPCs.
    pub client: &'a UreqHttpClient,
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Resolved HTTP(S) remote.
    pub remote: &'a RemoteUrl,
    /// Wanted object ids.
    pub wants: Vec<ObjectId>,
    /// Existing shallow boundary to replay.
    pub shallow: Vec<ObjectId>,
    /// Requested deepen depth, if this is a shallow fetch.
    pub deepen: Option<u32>,
    /// Whether to install the response as a promisor pack.
    pub promisor: bool,
}

pub fn install_fetch_pack_via_http_upload_pack(
    request: HttpFetchPackRequest<'_>,
    credentials: &mut dyn CredentialProvider,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    // A deepen request must always reach the server (the shallow boundary may move
    // even when every wanted object is already present), so only the plain fetch
    // takes the "everything is local already" shortcut.
    if request.deepen.is_none() && all_wants_present(&local_db, &request.wants)? {
        return Ok(Vec::new());
    }
    let upload_request = UploadPackRequest {
        wants: request.wants,
        capabilities: shallow_request_capabilities(request.deepen),
        shallow: request.shallow,
        deepen: request.deepen,
        ..UploadPackRequest::default()
    };
    let haves = crate::local::local_have_oids(request.git_dir, request.format)?;
    if request.deepen.is_none() {
        let mut response = http_upload_pack_post(
            request.client,
            request.remote,
            &upload_request,
            haves,
            credentials,
        )?;
        if request.promisor {
            install_upload_pack_raw_promisor_response_from_reader(
                request.format,
                &mut response.body,
                &local_db,
            )?;
        } else {
            install_upload_pack_raw_response_from_reader(
                request.format,
                &mut response.body,
                &local_db,
            )?;
        }
        return Ok(Vec::new());
    }

    let mut response = http_upload_pack_post(
        request.client,
        request.remote,
        &upload_request,
        haves,
        credentials,
    )?;
    let shallow_info = if request.promisor {
        let (shallow_info, _) = install_upload_pack_shallow_raw_promisor_response_from_reader(
            request.format,
            &mut response.body,
            &local_db,
        )?;
        shallow_info
    } else {
        let (shallow_info, _) = install_upload_pack_shallow_raw_response_from_reader(
            request.format,
            &mut response.body,
            &local_db,
        )?;
        shallow_info
    };
    Ok(shallow_info)
}

pub fn install_fetch_pack_via_http_protocol_v2_fetch(
    request: HttpFetchPackRequest<'_>,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    if request.deepen.is_none() && all_wants_present(&local_db, &request.wants)? {
        return Ok(Vec::new());
    }
    let haves = crate::local::local_have_oids(request.git_dir, request.format)?;
    let fetch = protocol_v2_fetch_request_from_upload_pack_semantics(
        request.wants,
        haves,
        request.shallow,
        request.deepen,
        handshake,
    )?;
    let sections = http_protocol_v2_fetch_response(
        request.client,
        request.remote,
        request.format,
        handshake,
        fetch,
        credentials,
    )?;
    let shallow_info = shallow_info_from_protocol_v2_fetch_sections(&sections);
    if request.promisor {
        install_protocol_v2_fetch_response_promisor_packfile(&sections, &local_db)?;
    } else {
        install_protocol_v2_fetch_response_packfile(&sections, &local_db)?;
    }
    Ok(shallow_info)
}

fn all_wants_present(db: &FileObjectDatabase, wants: &[ObjectId]) -> Result<bool> {
    for want in wants {
        if !db.contains(want)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The want-line capabilities to advertise for a fetch: the `shallow` capability
/// when a deepen is requested (git's upload-pack expects clients sending deepen to
/// negotiate shallow), otherwise none — preserving the existing plain-fetch wire
/// form exactly.
fn shallow_request_capabilities(deepen: Option<u32>) -> Vec<Capability> {
    if deepen.is_some() {
        vec![Capability {
            name: "shallow".into(),
            value: None,
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_protocol::{
        ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo, ProtocolV2LsRefsRecord,
        ProtocolVersion, RefAdvertisement, read_protocol_v2_fetch_response,
        write_protocol_v2_fetch_response, write_protocol_v2_ls_refs_response,
    };

    fn sample_v2_handshake() -> TransportHandshake {
        TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "ls-refs".into(),
                    value: Some("peel symrefs".into()),
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
        }
    }

    #[test]
    fn protocol_v2_ls_refs_command_request_includes_agent_and_object_format() {
        let handshake = sample_v2_handshake();
        let command = protocol_v2_ls_refs_command_request(ObjectFormat::Sha1, &handshake)
            .expect("test operation should succeed");
        assert_eq!(command.command, "ls-refs");
        assert_eq!(
            command.capabilities,
            vec![
                Capability {
                    name: "agent".into(),
                    value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ]
        );
        assert_eq!(
            ProtocolV2LsRefsRequest::from_command_request(&command)
                .expect("test operation should succeed"),
            ProtocolV2LsRefsRequest {
                peel: true,
                symrefs: true,
                unborn: false,
                ref_prefixes: vec!["HEAD".into(), "refs/heads/".into(), "refs/tags/".into(),],
            }
        );
    }

    #[test]
    fn protocol_v2_ls_refs_command_request_omits_object_format_when_unadvertised() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "ls-refs".into(),
                    value: None,
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
            ],
        };
        let command = protocol_v2_ls_refs_command_request(ObjectFormat::Sha1, &handshake)
            .expect("test operation should succeed");
        assert_eq!(
            command.capabilities,
            vec![Capability {
                name: "agent".into(),
                value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
            }]
        );
    }

    #[test]
    fn protocol_v2_ls_refs_round_trip_bridges_into_ref_advertisement_set() {
        let handshake = sample_v2_handshake();
        let command = protocol_v2_ls_refs_command_request(ObjectFormat::Sha1, &handshake)
            .expect("test operation should succeed");
        let head = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let tag = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let tag_peeled = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let records = vec![
            ProtocolV2LsRefsRecord::Ref(sley_protocol::ProtocolV2LsRefsRef {
                oid: head.clone(),
                name: "HEAD".into(),
                peeled: None,
                symref_target: Some("refs/heads/main".into()),
                attributes: Vec::new(),
            }),
            ProtocolV2LsRefsRecord::Ref(sley_protocol::ProtocolV2LsRefsRef {
                oid: head.clone(),
                name: "refs/heads/main".into(),
                peeled: None,
                symref_target: None,
                attributes: Vec::new(),
            }),
            ProtocolV2LsRefsRecord::Ref(sley_protocol::ProtocolV2LsRefsRef {
                oid: tag.clone(),
                name: "refs/tags/v1".into(),
                peeled: Some(tag_peeled.clone()),
                symref_target: None,
                attributes: Vec::new(),
            }),
        ];

        let mut request_body = Vec::new();
        write_protocol_v2_command_request(&mut request_body, &command)
            .expect("test operation should succeed");
        let mut response_body = Vec::new();
        write_protocol_v2_ls_refs_response(&mut response_body, &records)
            .expect("test operation should succeed");

        let set = read_protocol_v2_ls_refs_response_as_ref_advertisement_set(
            ObjectFormat::Sha1,
            &mut response_body.as_slice(),
        )
        .expect("test operation should succeed");
        assert_eq!(
            set,
            RefAdvertisementSet {
                protocol: ProtocolVersion::V2,
                refs: vec![
                    RefAdvertisement {
                        oid: head.clone(),
                        name: "HEAD".into(),
                        capabilities: vec![Capability {
                            name: "symref".into(),
                            value: Some("HEAD:refs/heads/main".into()),
                        }],
                    },
                    RefAdvertisement {
                        oid: head,
                        name: "refs/heads/main".into(),
                        capabilities: Vec::new(),
                    },
                    RefAdvertisement {
                        oid: tag,
                        name: "refs/tags/v1".into(),
                        capabilities: Vec::new(),
                    },
                    RefAdvertisement {
                        oid: tag_peeled,
                        name: "refs/tags/v1^{}".into(),
                        capabilities: Vec::new(),
                    },
                ],
                shallow: Vec::new(),
            }
        );
        assert!(!request_body.is_empty());
    }

    fn sample_v2_fetch_handshake() -> TransportHandshake {
        TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![
                Capability {
                    name: "fetch".into(),
                    value: Some("shallow sideband-all".into()),
                },
                Capability {
                    name: "agent".into(),
                    value: Some("git/2.54.0".into()),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ],
        }
    }

    #[test]
    fn protocol_v2_fetch_command_request_includes_agent_object_format_and_deepen() {
        let handshake = sample_v2_fetch_handshake();
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let have = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "3333333333333333333333333333333333333333",
        )
        .expect("test operation should succeed");
        let fetch = protocol_v2_fetch_request_from_upload_pack_semantics(
            vec![want.clone()],
            vec![have.clone()],
            vec![shallow.clone()],
            Some(3),
            &handshake,
        )
        .expect("test operation should succeed");
        assert!(fetch.sideband_all);
        assert!(fetch.done);
        let command = protocol_v2_fetch_command_request(ObjectFormat::Sha1, &handshake, &fetch)
            .expect("test operation should succeed");
        assert_eq!(command.command, "fetch");
        assert_eq!(
            command.capabilities,
            vec![
                Capability {
                    name: "agent".into(),
                    value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
                },
                Capability {
                    name: "object-format".into(),
                    value: Some("sha1".into()),
                },
            ]
        );
        assert_eq!(
            ProtocolV2FetchRequest::from_command_request(ObjectFormat::Sha1, &command)
                .expect("test operation should succeed"),
            ProtocolV2FetchRequest {
                wants: vec![want],
                haves: vec![have],
                shallow: vec![shallow],
                deepen: Some(3),
                done: true,
                sideband_all: true,
                ..ProtocolV2FetchRequest::default()
            }
        );
    }

    #[test]
    fn protocol_v2_fetch_round_trip_extracts_shallow_info_and_packfile_sections() {
        let handshake = sample_v2_fetch_handshake();
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("test operation should succeed");
        let shallow = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("test operation should succeed");
        let fetch = protocol_v2_fetch_request_from_upload_pack_semantics(
            vec![want],
            Vec::new(),
            vec![shallow.clone()],
            Some(1),
            &handshake,
        )
        .expect("test operation should succeed");
        let command = protocol_v2_fetch_command_request(ObjectFormat::Sha1, &handshake, &fetch)
            .expect("test operation should succeed");
        let mut request_body = Vec::new();
        write_protocol_v2_command_request(&mut request_body, &command)
            .expect("test operation should succeed");

        let sections = vec![
            ProtocolV2FetchResponseSection::ShallowInfo(vec![ProtocolV2FetchShallowInfo::Shallow(
                shallow,
            )]),
            ProtocolV2FetchResponseSection::Packfile(vec![b"PACK-test".to_vec()]),
        ];
        let mut response_body = Vec::new();
        write_protocol_v2_fetch_response(&mut response_body, &sections)
            .expect("test operation should succeed");
        let parsed =
            read_protocol_v2_fetch_response(ObjectFormat::Sha1, &mut response_body.as_slice())
                .expect("test operation should succeed");
        assert_eq!(parsed, sections);
        assert_eq!(
            shallow_info_from_protocol_v2_fetch_sections(&parsed),
            vec![ProtocolV2FetchShallowInfo::Shallow(
                ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .expect("test operation should succeed")
            )]
        );
        assert!(!request_body.is_empty());
    }
}
