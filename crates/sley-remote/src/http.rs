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

use std::io::Read;
use std::path::Path;

use crate::install::{
    ProgressInstaller, install_protocol_v2_packfile_from_reader_with_cancel,
    install_upload_pack_packfile_promisor_response_from_reader_with_cancel,
    install_upload_pack_packfile_response_from_reader_with_cancel,
    install_upload_pack_shallow_packfile_promisor_response_from_reader_with_cancel,
    install_upload_pack_shallow_packfile_response_from_reader_with_cancel,
    shallow_info_from_protocol_v2_fetch_header,
};
use sley_config::GitConfig;
use sley_core::{
    CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result, UPSTREAM_GIT_COMPAT_VERSION,
};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_protocol::{
    GitService, ProtocolV2CommandOptions, ProtocolV2CommandRequest, ProtocolV2FetchAcknowledgment,
    ProtocolV2FetchRequest, ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo,
    ProtocolV2FetchWantedRef, ProtocolV2LsRefsRequest, ProtocolVersion, RefAdvertisement,
    RefAdvertisementSet, TransportHandshake, UploadPackFeatures, UploadPackNegotiationRequest,
    UploadPackRequest, encode_protocol_v2_command_options, parse_protocol_v2_fetch_features,
    parse_upload_pack_features, protocol_v2_object_format,
    read_protocol_v2_fetch_negotiation_response, read_protocol_v2_fetch_response,
    read_protocol_v2_fetch_response_header, read_protocol_v2_fetch_sideband_all_response,
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set,
    smart_http_advertisement_content_type, smart_http_rpc_request_content_type,
    trace_protocol_v2_advertisement_read, validate_protocol_v2_fetch_command_request,
    validate_protocol_v2_ls_refs_command_request, write_protocol_v2_command_request,
    write_upload_pack_negotiation_request, write_upload_pack_request,
};
use sley_transport::{
    GitProtocolHeader, HttpClient, HttpResponse, RemoteTransport, RemoteUrl,
    ServiceDiscoveryPayload, ServiceDiscoveryResponse, UreqHttpClient, encode_git_protocol_header,
    git_credential_basic_authorization, http_smart_info_refs_url, http_smart_rpc_url,
    parse_remote_url, read_service_discovery_response,
};

use sley_protocol::{TransportLimits, read_to_end_bounded};

use crate::credentials::{credential_request_for_url, http_url_credential};
use crate::{CredentialProvider, ProgressSink};

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

/// Reusable HTTP client for every smart-HTTP RPC in one remote operation.
pub struct HttpOperationBatch {
    client: UreqHttpClient,
}

impl HttpOperationBatch {
    pub fn new() -> Self {
        Self {
            client: UreqHttpClient::new(),
        }
    }

    /// A batch whose client applies the ceilings `config` asks for.
    ///
    /// See [`transport_limits_from_config`]; with no relevant keys set this
    /// is exactly [`Self::new`].
    pub fn with_config(config: Option<&GitConfig>) -> Self {
        Self {
            client: UreqHttpClient::with_limits(transport_limits_from_config(config)),
        }
    }

    pub fn client(&self) -> &UreqHttpClient {
        &self.client
    }
}

impl Default for HttpOperationBatch {
    fn default() -> Self {
        Self::new()
    }
}

pub fn new_http_client() -> UreqHttpClient {
    UreqHttpClient::new()
}

/// [`new_http_client`] with the ceilings `config` asks for.
pub fn new_http_client_with_config(config: Option<&GitConfig>) -> UreqHttpClient {
    UreqHttpClient::with_limits(transport_limits_from_config(config))
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

/// Resolve the client protocol version for smart HTTP, defaulting to v2 like upstream git.
pub fn http_protocol_version_from_config(config: Option<&GitConfig>) -> Option<ProtocolVersion> {
    match config.and_then(|config| config.get("protocol", None, "version")) {
        Some("0") => Some(ProtocolVersion::V0),
        Some("1") => Some(ProtocolVersion::V1),
        Some("2") => Some(ProtocolVersion::V2),
        _ => Some(ProtocolVersion::V2),
    }
}

/// Encode the `Git-Protocol` request header value, if any (`None` for protocol v0).
pub fn http_git_protocol_header_value(config: Option<&GitConfig>) -> Result<Option<String>> {
    http_git_protocol_header_value_for_service(config, GitService::UploadPack)
}

/// Encode the `Git-Protocol` header for a specific smart-HTTP service.
///
/// Upstream `remote-curl.c` only negotiates protocol v2 for `git-upload-pack`;
/// push (`git-receive-pack`) and other RPCs fall back to v0.
pub fn http_git_protocol_header_value_for_service(
    config: Option<&GitConfig>,
    service: GitService,
) -> Result<Option<String>> {
    let mut version = http_protocol_version_from_config(config);
    if matches!(version, Some(ProtocolVersion::V2)) && service != GitService::UploadPack {
        version = Some(ProtocolVersion::V0);
    }
    match version {
        Some(ProtocolVersion::V0) => Ok(None),
        Some(version) => encode_git_protocol_header(&GitProtocolHeader {
            protocol: Some(version),
            extra_parameters: Vec::new(),
        }),
        None => Ok(None),
    }
}

/// Build smart-HTTP request headers for an optional credential and `Git-Protocol` value.
pub fn http_request_headers<'a>(
    auth: Option<&'a str>,
    git_protocol: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut headers = Vec::with_capacity(2);
    if let Some(value) = git_protocol {
        headers.push(("Git-Protocol", value));
    }
    if let Some(value) = auth {
        headers.push(("Authorization", value));
    }
    headers
}

/// Build the `Authorization` header list for an optional credential header value.
pub fn http_authorization_headers(auth: Option<&str>) -> Vec<(&str, &str)> {
    http_request_headers(auth, None)
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

/// Upload-pack discovery for a repository whose object format is not known yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUploadPackDiscovery {
    pub advertisements: HttpServiceAdvertisements,
    pub features: UploadPackFeatures,
    pub object_format: ObjectFormat,
}

/// Parse a smart-HTTP info/refs body into a ref advertisement set for protocol
/// v0/v1. Protocol v2 discovery responses require a follow-up `ls-refs` RPC; use
/// [`http_service_advertisements`] instead.
pub fn http_advertised_refs(
    format: ObjectFormat,
    response: HttpResponse,
) -> Result<RefAdvertisementSet> {
    http_advertised_refs_with_limits(format, response, TransportLimits::default())
}

/// [`http_advertised_refs`] with explicit ceilings.
pub fn http_advertised_refs_with_limits(
    format: ObjectFormat,
    mut response: HttpResponse,
    limits: TransportLimits,
) -> Result<RefAdvertisementSet> {
    let discovery =
        read_http_service_discovery_response_with_limits(format, &mut response.body, limits)?;
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

#[allow(clippy::too_many_arguments)]
fn protocol_v2_fetch_request_from_upload_pack_semantics(
    wants: Vec<ObjectId>,
    want_refs: Vec<String>,
    haves: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    deepen_since: Option<i64>,
    deepen_not: Vec<String>,
    deepen_relative: bool,
    filter: Option<&sley_odb::PackObjectFilter>,
    handshake: &TransportHandshake,
) -> Result<ProtocolV2FetchRequest> {
    let v2_features =
        parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    Ok(ProtocolV2FetchRequest {
        wants,
        want_refs,
        haves,
        shallow,
        deepen,
        deepen_since: deepen_since_u64(deepen_since),
        deepen_not,
        deepen_relative,
        filter: filter.and_then(crate::local::upload_pack_filter_protocol_spec),
        thin_pack: true,
        include_tag: true,
        ofs_delta: true,
        done: true,
        // Normal fetch negotiation expects the pack immediately after `ready`.
        // `wait-for-done` is reserved for callers such as `--negotiate-only`.
        wait_for_done: false,
        sideband_all: v2_features.sideband_all,
        ..ProtocolV2FetchRequest::default()
    })
}

fn deepen_since_u64(deepen_since: Option<i64>) -> Option<u64> {
    deepen_since.and_then(|value| u64::try_from(value).ok())
}

fn build_http_upload_pack_request(
    wants: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    deepen_since: Option<i64>,
    deepen_not: Vec<String>,
    filter: Option<&sley_odb::PackObjectFilter>,
) -> UploadPackRequest {
    UploadPackRequest {
        wants,
        capabilities: upload_pack_request_capabilities(
            deepen,
            filter
                .and_then(crate::local::upload_pack_filter_protocol_spec)
                .is_some(),
        ),
        shallow,
        deepen,
        deepen_since: deepen_since_u64(deepen_since),
        deepen_not,
        filter: filter.and_then(crate::local::upload_pack_filter_protocol_spec),
    }
}

fn upload_pack_request_capabilities(deepen: Option<u32>, filter: bool) -> Vec<Capability> {
    let mut capabilities = Vec::new();
    // The v0 upload-pack response reader demuxes a side-band-64k stream, so the
    // request must negotiate it; otherwise the server streams a bare packfile
    // and the reader mis-parses the leading `PACK` signature as a pkt-line
    // length ("invalid pkt-line length byte 0x50"). Capabilities not available
    // in this request type are omitted rather than being requested without an
    // advertisement; in particular, Sley's server does not advertise
    // `ofs-delta` yet.
    capabilities.push(Capability {
        name: "side-band-64k".into(),
        value: None,
    });
    if deepen.is_some() {
        capabilities.push(Capability {
            name: "shallow".into(),
            value: None,
        });
    }
    if filter {
        capabilities.push(Capability {
            name: "filter".into(),
            value: None,
        });
    }
    capabilities
}

fn request_replays_shallow_boundary(
    deepen: Option<u32>,
    deepen_since: Option<i64>,
    deepen_not: &[String],
) -> bool {
    deepen.is_some() || deepen_since.is_some() || !deepen_not.is_empty()
}

fn http_protocol_v2_ls_refs_advertisements<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    service: GitService,
    handshake: TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    git_protocol: Option<&str>,
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
            &http_request_headers(auth, git_protocol),
            &body,
        )
    })?;
    http_check_status(&response, &url)?;
    read_protocol_v2_ls_refs_response_as_ref_advertisement_set(format, &mut response.body)
}

/// Fetch and parse the ref advertisements for `service` from the smart-HTTP
/// info/refs endpoint, authenticating and validating status + content type.
pub fn http_service_advertisements<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    service: GitService,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<HttpServiceAdvertisements> {
    http_service_advertisements_with_expected_format(
        client,
        remote,
        Some(format),
        service,
        credentials,
        config,
    )
}

fn http_service_advertisements_with_expected_format<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    expected_format: Option<ObjectFormat>,
    service: GitService,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<HttpServiceAdvertisements> {
    // Git's smart-HTTP packet stream is owned by the logical remote-curl
    // helper, whose packet trace identity is `git` even though Sley performs
    // the same operation in-process. Keep this observable boundary without
    // spawning `git-remote-http(s)` or consulting an installed Git.
    let _packet_trace_identity = sley_protocol::scoped_packet_trace_identity("git");
    let git_protocol = http_git_protocol_header_value_for_service(config, service)?;
    let url = http_smart_info_refs_url(remote, service)?;
    let mut response = http_send_with_auth(remote, credentials, |auth| {
        client.get(&url, &http_request_headers(auth, git_protocol.as_deref()))
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(&response, &smart_http_advertisement_content_type(service)?)?;
    // The advertisement ceiling follows the same config the protocol version
    // does, so an operator who has to raise it does not also have to rebuild.
    let discovery = read_http_service_discovery_response_with_limits(
        expected_format.unwrap_or(ObjectFormat::Sha1),
        &mut response.body,
        transport_limits_from_config(config),
    )?;
    let object_format = service_discovery_object_format(&discovery)?;
    if let Some(expected) = expected_format
        && object_format != expected
    {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            object_format.name(),
            expected.name()
        )));
    }
    match discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(HttpServiceAdvertisements {
            set,
            handshake: None,
        }),
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            let set = http_protocol_v2_ls_refs_advertisements(
                client,
                remote,
                object_format,
                service,
                handshake.clone(),
                credentials,
                git_protocol.as_deref(),
            )?;
            Ok(HttpServiceAdvertisements {
                set,
                handshake: Some(handshake),
            })
        }
    }
}

fn service_discovery_object_format(discovery: &ServiceDiscoveryResponse) -> Result<ObjectFormat> {
    match &discovery.payload {
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            protocol_v2_object_format(&handshake.capabilities)
        }
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(set
            .refs
            .first()
            .map(|reference| parse_upload_pack_features(&reference.capabilities))
            .transpose()?
            .and_then(|features| features.object_format)
            .unwrap_or(ObjectFormat::Sha1)),
    }
}

/// Discover upload-pack capabilities and object format before creating a clone.
pub fn http_discover_upload_pack<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<HttpUploadPackDiscovery> {
    let advertisements = http_service_advertisements_with_expected_format(
        client,
        remote,
        None,
        GitService::UploadPack,
        credentials,
        config,
    )?;
    let features =
        http_upload_pack_features(&advertisements.set.refs, advertisements.handshake.as_ref())?;
    let object_format = features.object_format.unwrap_or(ObjectFormat::Sha1);
    Ok(HttpUploadPackDiscovery {
        advertisements,
        features,
        object_format,
    })
}

/// Read a smart-HTTP service-discovery response under an explicit ceiling.
///
/// There is deliberately no ceiling-free variant: every caller has the git
/// config in scope, so the bound always follows configuration rather than a
/// compiled-in constant. Tests pass a small ceiling to exercise the bound
/// without materialising a 128 MiB advertisement.
fn read_http_service_discovery_response_with_limits(
    format: ObjectFormat,
    reader: &mut impl Read,
    limits: TransportLimits,
) -> Result<ServiceDiscoveryResponse> {
    let bytes = read_to_end_bounded(reader, limits.ref_advertisement())?;
    let first = read_service_discovery_response(format, &mut bytes.as_slice());
    let alternate = match format {
        ObjectFormat::Sha1 => ObjectFormat::Sha256,
        ObjectFormat::Sha256 => ObjectFormat::Sha1,
    };
    match first {
        Ok(discovery) => Ok(discovery),
        Err(original) => {
            let retry = read_service_discovery_response(alternate, &mut bytes.as_slice());
            match retry {
                Ok(discovery) if discovery_advertises_object_format(&discovery, alternate) => {
                    Ok(discovery)
                }
                _ => Err(original),
            }
        }
    }
}

fn discovery_advertises_object_format(
    discovery: &ServiceDiscoveryResponse,
    format: ObjectFormat,
) -> bool {
    match &discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => set.refs.first().is_some_and(|reference| {
            reference.capabilities.iter().any(|capability| {
                capability.name == "object-format"
                    && capability.value.as_deref() == Some(format.name())
            })
        }),
        ServiceDiscoveryPayload::ProtocolV2(handshake) => {
            handshake.capabilities.iter().any(|capability| {
                capability.name == "object-format"
                    && capability.value.as_deref() == Some(format.name())
            })
        }
    }
}

/// The upload-pack ref advertisements and parsed features for `remote`.
pub fn http_upload_pack_advertisements<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    let discovered = http_service_advertisements(
        client,
        remote,
        format,
        GitService::UploadPack,
        credentials,
        config,
    )?;
    let features = http_upload_pack_features(&discovered.set.refs, discovered.handshake.as_ref())?;
    Ok((discovered.set.refs, features))
}

/// Bridge protocol v2 handshake capabilities (filter/shallow/…) and v0/v1 ref
/// advertisement capabilities into [`UploadPackFeatures`].
pub fn http_upload_pack_features(
    advertisements: &[RefAdvertisement],
    handshake: Option<&TransportHandshake>,
) -> Result<UploadPackFeatures> {
    if let Some(handshake) = handshake {
        let v2 = parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
        let mut features = UploadPackFeatures {
            object_format: Some(protocol_v2_object_format(&handshake.capabilities)?),
            shallow: v2.shallow,
            deepen_since: v2.shallow,
            deepen_not: v2.shallow,
            filter: v2.filter,
            ..UploadPackFeatures::default()
        };
        if let Some(first) = advertisements.first() {
            let bridged = parse_upload_pack_features(&first.capabilities)?;
            features.symrefs = bridged.symrefs;
        }
        return Ok(features);
    }
    Ok(advertisements
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default())
}

/// Post an upload-pack RPC `request` + `haves` and return the validated HTTP
/// response with its body still unread, so the caller can parse the packfile
/// stream (with or without a leading shallow-info section). Authenticates and
/// validates status; like Git's RPC path, the response body is parsed without
/// enforcing a response content type.
fn http_upload_pack_post<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    request: &UploadPackRequest,
    haves: Vec<ObjectId>,
    credentials: &mut dyn CredentialProvider,
    git_protocol: Option<&str>,
    post_buffer: usize,
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
        http_post_rpc_body(
            client,
            &url,
            &content_type,
            &http_request_headers(auth, git_protocol),
            &body,
            post_buffer,
        )
    })?;
    http_check_status(&response, &url)?;
    // Git validates the discovery response's content type, but its stateless
    // RPC path streams a successful POST body directly into the pkt-line
    // parser. Preserve that distinction so malformed-response diagnostics
    // report the wire truncation rather than being masked by an unusual CGI
    // content type.
    Ok(response)
}

/// Post a protocol v2 `fetch` RPC with `wants`/`haves`/`shallow`/`deepen` and
/// read back the sectioned response. Authenticates and validates status. When
/// the server advertises `sideband-all`, the request and response use the
/// sideband-all wire form.
pub fn http_protocol_v2_fetch_response<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    handshake: &TransportHandshake,
    fetch: ProtocolV2FetchRequest,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let git_protocol = http_git_protocol_header_value(config)?;
    let post_buffer = http_post_buffer(config);
    let sideband_all = fetch.sideband_all;
    let mut response = http_protocol_v2_fetch_post(
        client,
        remote,
        format,
        handshake,
        fetch,
        credentials,
        HttpRpcOptions {
            git_protocol: git_protocol.as_deref(),
            post_buffer,
        },
    )?;
    if sideband_all {
        Ok(read_protocol_v2_fetch_sideband_all_response(format, &mut response.body)?.sections)
    } else {
        read_protocol_v2_fetch_response(format, &mut response.body)
    }
}

#[derive(Clone, Copy)]
struct HttpRpcOptions<'a> {
    git_protocol: Option<&'a str>,
    post_buffer: usize,
}

fn http_protocol_v2_fetch_post<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    handshake: &TransportHandshake,
    fetch: ProtocolV2FetchRequest,
    credentials: &mut dyn CredentialProvider,
    options: HttpRpcOptions<'_>,
) -> Result<HttpResponse> {
    let command = protocol_v2_fetch_command_request(format, handshake, &fetch)?;
    let url = http_smart_rpc_url(remote, GitService::UploadPack)?;
    let mut body = Vec::new();
    write_protocol_v2_command_request(&mut body, &command)?;
    let content_type = smart_http_rpc_request_content_type(GitService::UploadPack)?;
    let response = http_send_with_auth(remote, credentials, |auth| {
        http_post_rpc_body(
            client,
            &url,
            &content_type,
            &http_request_headers(auth, options.git_protocol),
            &body,
            options.post_buffer,
        )
    })?;
    http_check_status(&response, &url)?;
    Ok(response)
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
pub struct HttpFetchPackRequest<'a, C: HttpClient + ?Sized> {
    /// HTTP client used for smart-HTTP RPCs. Generic over [`HttpClient`] so a host
    /// can inject a network-policy-enforcing client (e.g. an SSRF guard); the
    /// default fetch/clone path uses [`UreqHttpClient`].
    pub client: &'a C,
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Resolved HTTP(S) remote.
    pub remote: &'a RemoteUrl,
    /// Wanted object ids.
    pub wants: Vec<ObjectId>,
    /// Caller-selected negotiation haves. `None` means advertise the default
    /// local haves.
    pub haves: Option<Vec<ObjectId>>,
    /// Existing shallow boundary to replay.
    pub shallow: Vec<ObjectId>,
    /// Requested deepen depth, if this is a shallow fetch.
    pub deepen: Option<u32>,
    /// Whether to install the response as a promisor pack.
    pub promisor: bool,
    /// Maximum raw pack bytes to accept from the remote (`fetch.maxInputSize` /
    /// `transfer.maxSize`). `None` means unlimited.
    pub max_input_size: Option<u64>,
    pub filter: Option<sley_odb::PackObjectFilter>,
    pub deepen_since: Option<i64>,
    pub deepen_not: Vec<String>,
    pub deepen_relative: bool,
    pub git_protocol: Option<&'a str>,
    /// Maximum buffered smart-HTTP request size (`http.postBuffer`). Larger
    /// request bodies use chunked transfer encoding.
    pub post_buffer: usize,
    /// Send no `have` lines. Used by a partial clone's checkout-blob top-up
    /// fetch: the client already has the commit whose tree references the wanted
    /// blob, so advertising it as a `have` would make the server treat the blob
    /// as already transferred (reachable from the have) and omit it. Suppressing
    /// haves forces the server to send the explicitly wanted objects.
    pub omit_haves: bool,
}

/// A negotiated protocol-v2 fetch response whose metadata header has been
/// consumed.
///
/// When [`has_packfile`](Self::has_packfile) is true, [`body`](Self::body) is
/// positioned immediately after the `packfile` section marker. The remaining
/// bytes are a pure sideband stream; wrap the body in
/// [`sley_protocol::StreamingSidebandReader`] to consume raw `PACK` bytes
/// without installing them into a local repository.
pub struct NegotiatedPackResponse {
    /// Successful smart-HTTP response body positioned at the packfile payload.
    pub body: Box<dyn Read + Send>,
    /// Shallow-boundary updates parsed before the packfile section.
    pub shallow_info: Vec<ProtocolV2FetchShallowInfo>,
    /// Ref names and object ids resolved by protocol-v2 `want-ref`.
    pub wanted_refs: Vec<ProtocolV2FetchWantedRef>,
    /// Whether the response contains a packfile section.
    pub has_packfile: bool,
}

/// Outcome of a protocol-v2 HTTP fetch, including any `wanted-refs` the server
/// resolved for `want-ref` lines (so the client can update tracking refs to the
/// OIDs current at request time rather than the earlier ls-refs snapshot).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpProtocolV2FetchOutcome {
    pub shallow_info: Vec<ProtocolV2FetchShallowInfo>,
    pub wanted_refs: Vec<ProtocolV2FetchWantedRef>,
}

pub fn install_fetch_pack_via_http_upload_pack<C: HttpClient + ?Sized>(
    request: HttpFetchPackRequest<'_, C>,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if request.wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    // A deepen request must always reach the server (the shallow boundary may move
    // even when every wanted object is already present), so only the plain fetch
    // takes the "everything is local already" shortcut.
    if !request_replays_shallow_boundary(request.deepen, request.deepen_since, &request.deepen_not)
        && request.filter.is_none()
        && all_wants_present(&local_db, &request.wants)?
    {
        return Ok(Vec::new());
    }
    let upload_request = build_http_upload_pack_request(
        request.wants,
        request.shallow,
        request.deepen,
        request.deepen_since,
        request.deepen_not,
        request.filter.as_ref(),
    );
    let haves = request_haves(
        request.git_dir,
        request.format,
        request.omit_haves,
        request.haves.clone(),
    )?;
    if request.deepen.is_none() {
        let mut response = http_upload_pack_post(
            request.client,
            request.remote,
            &upload_request,
            haves,
            credentials,
            request.git_protocol,
            request.post_buffer,
        )?;
        if request.promisor {
            install_upload_pack_packfile_promisor_response_from_reader_with_cancel(
                request.format,
                &mut response.body,
                &local_db,
                request.max_input_size,
                cancel,
            )?;
        } else {
            install_upload_pack_packfile_response_from_reader_with_cancel(
                request.format,
                &mut response.body,
                &ProgressInstaller::new(&local_db, progress),
                request.max_input_size,
                cancel,
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
        request.git_protocol,
        request.post_buffer,
    )?;
    let shallow_info = if request.promisor {
        let (shallow_info, _) =
            install_upload_pack_shallow_packfile_promisor_response_from_reader_with_cancel(
                request.format,
                &mut response.body,
                &local_db,
                request.max_input_size,
                cancel,
            )?;
        shallow_info
    } else {
        let (shallow_info, _) =
            install_upload_pack_shallow_packfile_response_from_reader_with_cancel(
                request.format,
                &mut response.body,
                &ProgressInstaller::new(&local_db, progress),
                request.max_input_size,
                cancel,
            )?;
        shallow_info
    };
    Ok(shallow_info)
}

/// Protocol-v2 `--negotiate-only` over smart HTTP.
///
/// Discovers the upload-pack advertisement, requires the `wait-for-done` fetch
/// feature, POSTs a `fetch` command with only haves, and returns the ACKed
/// common object ids.
pub fn negotiate_only_http<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    tip_oids: &[ObjectId],
    git_dir: &Path,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<Vec<ObjectId>> {
    let discovery = http_discover_upload_pack(client, remote, credentials, config)?;
    let Some(handshake) = discovery.advertisements.handshake.as_ref() else {
        eprintln!("warning: --negotiate-only requires protocol v2");
        return Err(GitError::Exit(1));
    };
    if handshake.protocol != ProtocolVersion::V2 {
        eprintln!("warning: --negotiate-only requires protocol v2");
        return Err(GitError::Exit(1));
    }
    let features = parse_protocol_v2_fetch_features(&handshake.capabilities)?.unwrap_or_default();
    if !features.wait_for_done {
        eprintln!("warning: server does not support wait-for-done");
        return Err(GitError::Exit(1));
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut seen = std::collections::HashSet::new();
    let mut haves = Vec::new();
    for tip in tip_oids {
        let mut stack = vec![*tip];
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            haves.push(oid);
            let Ok(object) = local_db.read_object(&oid) else {
                continue;
            };
            if object.object_type != sley_object::ObjectType::Commit {
                continue;
            }
            if let Ok(commit) = sley_object::Commit::parse_ref(format, &object.body) {
                for parent in commit.parents {
                    if !seen.contains(&parent) {
                        stack.push(parent);
                    }
                }
            }
        }
    }
    let fetch = ProtocolV2FetchRequest {
        haves,
        wait_for_done: true,
        done: false,
        thin_pack: true,
        ofs_delta: true,
        include_tag: false,
        ..ProtocolV2FetchRequest::default()
    };
    let git_protocol = http_git_protocol_header_value(config)?;
    let mut response = http_protocol_v2_fetch_post(
        client,
        remote,
        format,
        handshake,
        fetch,
        credentials,
        HttpRpcOptions {
            git_protocol: git_protocol.as_deref(),
            post_buffer: 1_048_576,
        },
    )?;
    let negotiation =
        read_protocol_v2_fetch_negotiation_response(format, &mut response.body, false, true)?;
    let mut acked = Vec::new();
    for ack in negotiation.acknowledgments {
        if let ProtocolV2FetchAcknowledgment::Ack(oid) = ack {
            acked.push(oid);
        }
    }
    Ok(acked)
}

pub fn install_fetch_pack_via_http_protocol_v2_fetch<C: HttpClient + ?Sized>(
    request: HttpFetchPackRequest<'_, C>,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    install_fetch_pack_via_http_protocol_v2_fetch_with_want_refs(
        request,
        Vec::new(),
        handshake,
        credentials,
        progress,
        cancel,
    )
    .map(|outcome| outcome.shallow_info)
}

/// Install a protocol-v2 HTTP fetch while resolving `want-ref` names at request
/// time. Keeping the names separate from [`HttpFetchPackRequest`] preserves the
/// exact-OID request API used by independent partial-clone callers.
pub fn install_fetch_pack_via_http_protocol_v2_fetch_with_want_refs<C: HttpClient + ?Sized>(
    mut request: HttpFetchPackRequest<'_, C>,
    want_refs: Vec<String>,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
    cancel: CancelFlag<'_>,
) -> Result<HttpProtocolV2FetchOutcome> {
    if request.wants.is_empty() && want_refs.is_empty() {
        return Ok(HttpProtocolV2FetchOutcome::default());
    }
    trace_protocol_v2_advertisement_read(handshake)?;
    let local_db = FileObjectDatabase::from_git_dir(request.git_dir, request.format);
    // When using want-ref, the advertised OIDs may be stale/wrong; always talk to
    // the server so it can resolve names at request time.
    if want_refs.is_empty()
        && !request_replays_shallow_boundary(
            request.deepen,
            request.deepen_since,
            &request.deepen_not,
        )
        && request.filter.is_none()
        && all_wants_present(&local_db, &request.wants)?
    {
        return Ok(HttpProtocolV2FetchOutcome::default());
    }
    let haves = request_negotiation_haves(
        request.git_dir,
        request.format,
        request.omit_haves,
        request.haves.clone(),
    )?;
    request.haves = Some(haves);
    let promisor = request.promisor;
    let max_input_size = request.max_input_size;
    let mut negotiated = negotiate_fetch_pack_via_http_protocol_v2_after_trace(
        request,
        want_refs,
        handshake,
        credentials,
        cancel,
    )?;
    if !negotiated.has_packfile {
        return Ok(HttpProtocolV2FetchOutcome {
            shallow_info: negotiated.shallow_info,
            wanted_refs: negotiated.wanted_refs,
        });
    }
    if promisor {
        install_protocol_v2_packfile_from_reader_with_cancel(
            &mut negotiated.body,
            &local_db,
            true,
            max_input_size,
            cancel,
        )?;
    } else {
        install_protocol_v2_packfile_from_reader_with_cancel(
            &mut negotiated.body,
            &ProgressInstaller::new(&local_db, progress),
            false,
            max_input_size,
            cancel,
        )?;
    }
    Ok(HttpProtocolV2FetchOutcome {
        shallow_info: negotiated.shallow_info,
        wanted_refs: negotiated.wanted_refs,
    })
}

/// Negotiate a multi-round protocol-v2 smart-HTTP fetch without accessing or
/// installing into a local repository.
///
/// The caller must provide an explicit have set in
/// [`HttpFetchPackRequest::haves`], including `Some(Vec::new())` when it has no
/// haves. `git_dir`, `promisor`, and `max_input_size` are intentionally ignored:
/// embedders can use this operation without a local `.git` repository and own
/// the policy for consuming the returned pack stream.
///
/// The negotiation starts with 16 haves and doubles the advertised prefix each
/// round. It sends `done` after the server reports `ready` or the have set is
/// exhausted, matching the repository-installing HTTP fetch path.
pub fn negotiate_fetch_pack_via_http_protocol_v2<C: HttpClient + ?Sized>(
    request: HttpFetchPackRequest<'_, C>,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    cancel: CancelFlag<'_>,
) -> Result<NegotiatedPackResponse> {
    if request.wants.is_empty() {
        return Err(GitError::InvalidFormat(
            "protocol v2 fetch negotiation requires at least one want".into(),
        ));
    }
    trace_protocol_v2_advertisement_read(handshake)?;
    negotiate_fetch_pack_via_http_protocol_v2_after_trace(
        request,
        Vec::new(),
        handshake,
        credentials,
        cancel,
    )
}

fn negotiate_fetch_pack_via_http_protocol_v2_after_trace<C: HttpClient + ?Sized>(
    mut request: HttpFetchPackRequest<'_, C>,
    want_refs: Vec<String>,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    cancel: CancelFlag<'_>,
) -> Result<NegotiatedPackResponse> {
    let haves = request.haves.take().ok_or_else(|| {
        GitError::InvalidFormat(
            "negotiation-only protocol v2 HTTP fetch requires explicit haves".into(),
        )
    })?;
    let haves = if request.omit_haves {
        Vec::new()
    } else {
        haves
    };
    let fetch = protocol_v2_fetch_request_from_upload_pack_semantics(
        request.wants,
        want_refs,
        haves,
        request.shallow,
        request.deepen,
        request.deepen_since,
        request.deepen_not,
        request.deepen_relative,
        request.filter.as_ref(),
        handshake,
    )?;
    let sideband_all = fetch.sideband_all;
    let mut response = negotiate_protocol_v2_fetch_rounds(
        request.client,
        request.remote,
        request.format,
        handshake,
        fetch,
        credentials,
        HttpRpcOptions {
            git_protocol: request.git_protocol,
            post_buffer: request.post_buffer,
        },
        cancel,
    )?;
    let header =
        read_protocol_v2_fetch_response_header(request.format, &mut response.body, sideband_all)?;
    let mut wanted_refs = Vec::new();
    for section in &header.sections {
        if let ProtocolV2FetchResponseSection::WantedRefs(wanted) = section {
            wanted_refs.extend(wanted.iter().cloned());
        }
    }
    Ok(NegotiatedPackResponse {
        body: response.body,
        shallow_info: shallow_info_from_protocol_v2_fetch_header(&header),
        wanted_refs,
        has_packfile: header.has_packfile,
    })
}

#[allow(clippy::too_many_arguments)]
fn negotiate_protocol_v2_fetch_rounds<C: HttpClient + ?Sized>(
    client: &C,
    remote: &RemoteUrl,
    format: ObjectFormat,
    handshake: &TransportHandshake,
    mut fetch: ProtocolV2FetchRequest,
    credentials: &mut dyn CredentialProvider,
    options: HttpRpcOptions<'_>,
    cancel: CancelFlag<'_>,
) -> Result<HttpResponse> {
    let sideband_all = fetch.sideband_all;
    let all_haves = std::mem::take(&mut fetch.haves);
    const INITIAL_HAVE_BATCH: usize = 16;
    let mut sent_haves = all_haves.len().min(INITIAL_HAVE_BATCH);
    fetch.haves = all_haves[..sent_haves].to_vec();
    fetch.done = all_haves.is_empty();

    loop {
        // M0: poll cancel between negotiation rounds (not only during pack I/O).
        cancel.check()?;
        let sent_done = fetch.done;
        let wait_for_done = fetch.wait_for_done;
        let mut response = http_protocol_v2_fetch_post(
            client,
            remote,
            format,
            handshake,
            fetch.clone(),
            credentials,
            options,
        )?;

        let has_packfile = if sent_done {
            true
        } else {
            let negotiation = read_protocol_v2_fetch_negotiation_response(
                format,
                &mut response.body,
                sideband_all,
                wait_for_done,
            )?;
            if negotiation.has_following_sections {
                true
            } else {
                let ready = negotiation
                    .acknowledgments
                    .iter()
                    .any(|ack| matches!(ack, ProtocolV2FetchAcknowledgment::Ready));
                if ready || sent_haves == all_haves.len() {
                    fetch.done = true;
                    fetch.haves = all_haves.clone();
                } else {
                    sent_haves = (sent_haves * 2).min(all_haves.len());
                    fetch.haves = all_haves[..sent_haves].to_vec();
                }
                false
            }
        };
        if !has_packfile {
            continue;
        }
        return Ok(response);
    }
}

/// Resolve the buffered-response ceilings from git config.
///
/// The keys, all optional, all sizes in git's usual `k`/`m`/`g` notation:
///
/// * `sley.maxRefAdvertisementBytes` -- ceiling on a buffered v0/v1 reference
///   advertisement (default 128 MiB, about 512Ki refs). The remedy of first
///   resort for a repository with more refs than that is protocol v2
///   `ls-refs` with a `ref-prefix`, which sley already uses by default for
///   `git-upload-pack`; this key exists for the paths that cannot, chiefly
///   `git-receive-pack` and servers without v2.
/// * `sley.maxPackfileResponseBytes` -- ceiling on a packfile-bearing
///   response buffered whole (default 4 GiB). The body deadline is derived
///   from it, so raising it raises the time budget that pays for it.
/// * `sley.minTransferBytesPerSec` -- the slowest average rate served
///   (default 1 MiB/s), the other half of that derivation.
///
/// Every value is clamped by [`TransportLimits::clamped`]: an unset, zero or
/// unparseable value falls back to the default, and no value can raise a
/// ceiling past `sley_protocol::MAX_CONFIGURABLE_RESPONSE_BYTES` or derive a
/// deadline past `sley_protocol::MAX_BODY_TRANSFER_TIMEOUT`. Configuration
/// moves these bounds; it cannot remove them.
pub fn transport_limits_from_config(config: Option<&GitConfig>) -> TransportLimits {
    let defaults = TransportLimits::default();
    let size = |key: &str, fallback: u64| -> u64 {
        config
            .and_then(|config| config.get("sley", None, key))
            .and_then(sley_config::parse_config_int)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .filter(|bytes| *bytes > 0)
            .unwrap_or(fallback)
    };
    TransportLimits {
        max_ref_advertisement_bytes: size(
            "maxRefAdvertisementBytes",
            defaults.max_ref_advertisement_bytes,
        ),
        max_packfile_response_bytes: size(
            "maxPackfileResponseBytes",
            defaults.max_packfile_response_bytes,
        ),
        min_transfer_bytes_per_sec: size(
            "minTransferBytesPerSec",
            defaults.min_transfer_bytes_per_sec,
        ),
    }
    .clamped()
}

pub(crate) fn http_post_buffer(config: Option<&GitConfig>) -> usize {
    const DEFAULT_HTTP_POST_BUFFER: usize = 1 << 20;
    config
        .and_then(|config| config.get("http", None, "postBuffer"))
        .and_then(sley_config::parse_config_int)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(DEFAULT_HTTP_POST_BUFFER)
}

fn http_post_rpc_body<C: HttpClient + ?Sized>(
    client: &C,
    url: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    post_buffer: usize,
) -> Result<HttpResponse> {
    if body.len() > post_buffer {
        let mut reader = std::io::Cursor::new(body);
        client.post_reader(url, content_type, headers, &mut reader)
    } else {
        client.post(url, content_type, headers, body)
    }
}

fn all_wants_present(db: &FileObjectDatabase, wants: &[ObjectId]) -> Result<bool> {
    for want in wants {
        if !db.contains(want)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn request_haves(
    git_dir: &Path,
    format: ObjectFormat,
    omit_haves: bool,
    custom_haves: Option<Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if omit_haves {
        Ok(Vec::new())
    } else if let Some(haves) = custom_haves {
        Ok(haves)
    } else {
        crate::local::local_have_oids(git_dir, format)
    }
}

fn request_negotiation_haves(
    git_dir: &Path,
    format: ObjectFormat,
    omit_haves: bool,
    custom_haves: Option<Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if omit_haves {
        Ok(Vec::new())
    } else if let Some(haves) = custom_haves {
        Ok(haves)
    } else {
        crate::local::local_negotiation_have_oids(git_dir, format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_protocol::{
        ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo, ProtocolV2LsRefsRecord,
        ProtocolVersion, RefAdvertisement, StreamingSidebandReader,
        read_protocol_v2_command_request, read_protocol_v2_fetch_response,
        write_protocol_v2_fetch_response, write_protocol_v2_fetch_sideband_all_response,
        write_protocol_v2_ls_refs_response,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn protocol_v1_header_is_sent_for_upload_and_receive_discovery() {
        let config = GitConfig::parse(b"[protocol]\n\tversion = 1\n").expect("config");
        assert_eq!(
            http_git_protocol_header_value_for_service(Some(&config), GitService::UploadPack)
                .expect("upload-pack header")
                .as_deref(),
            Some("version=1")
        );
        assert_eq!(
            http_git_protocol_header_value_for_service(Some(&config), GitService::ReceivePack)
                .expect("receive-pack header")
                .as_deref(),
            Some("version=1")
        );

        let config = GitConfig::parse(b"[protocol]\n\tversion = 2\n").expect("config");
        assert_eq!(
            http_git_protocol_header_value_for_service(Some(&config), GitService::ReceivePack)
                .expect("receive-pack fallback"),
            None
        );
    }

    struct PostModeClient {
        mode: Mutex<Option<&'static str>>,
    }

    impl PostModeClient {
        fn response() -> Result<HttpResponse> {
            Ok(HttpResponse {
                status: 200,
                content_type: None,
                body: Box::new(std::io::empty()),
            })
        }
    }

    impl HttpClient for PostModeClient {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
            Self::response()
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<HttpResponse> {
            *self.mode.lock().expect("post mode") = Some("buffered");
            Self::response()
        }

        fn post_reader(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            body: &mut dyn Read,
        ) -> Result<HttpResponse> {
            let mut consumed = Vec::new();
            body.read_to_end(&mut consumed)?;
            *self.mode.lock().expect("post mode") = Some("chunked");
            Self::response()
        }
    }

    #[test]
    fn http_post_buffer_selects_buffered_or_chunked_rpc_body() {
        let config = GitConfig::parse(b"[http]\n\tpostBuffer = 64k\n").expect("config");
        assert_eq!(http_post_buffer(Some(&config)), 64 * 1024);

        let client = PostModeClient {
            mode: Mutex::new(None),
        };
        http_post_rpc_body(
            &client,
            "http://example.test/rpc",
            "request",
            &[],
            b"1234",
            4,
        )
        .expect("buffered post");
        assert_eq!(*client.mode.lock().expect("mode"), Some("buffered"));

        http_post_rpc_body(
            &client,
            "http://example.test/rpc",
            "request",
            &[],
            b"12345",
            4,
        )
        .expect("chunked post");
        assert_eq!(*client.mode.lock().expect("mode"), Some("chunked"));
    }

    #[test]
    fn v1_upload_request_omits_unadvertised_ofs_delta() {
        let capabilities = upload_pack_request_capabilities(None, false);
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.name == "side-band-64k")
        );
        assert!(
            capabilities
                .iter()
                .all(|capability| capability.name != "ofs-delta")
        );
    }

    #[test]
    fn v1_discovery_adopts_explicit_sha256_object_format() {
        let response = ServiceDiscoveryResponse {
            announcement: sley_transport::ServiceAnnouncement {
                service: GitService::UploadPack,
            },
            payload: ServiceDiscoveryPayload::AdvertisedRefs(RefAdvertisementSet {
                protocol: ProtocolVersion::V1,
                refs: vec![RefAdvertisement {
                    oid: ObjectId::null(ObjectFormat::Sha256),
                    name: "capabilities^{}".into(),
                    capabilities: vec![Capability {
                        name: "object-format".into(),
                        value: Some("sha256".into()),
                    }],
                }],
                shallow: Vec::new(),
            }),
        };
        let mut encoded = Vec::new();
        sley_transport::write_service_discovery_response(&mut encoded, &response)
            .expect("discovery response");

        let parsed = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut encoded.as_slice(),
            TransportLimits::default(),
        )
        .expect("adaptive discovery");
        let ServiceDiscoveryPayload::AdvertisedRefs(set) = parsed.payload else {
            panic!("expected advertised refs");
        };
        assert_eq!(set.protocol, ProtocolVersion::V1);
        assert_eq!(set.refs[0].oid.format(), ObjectFormat::Sha256);
    }

    /// An [`HttpClient`] double that records POST dials and returns a canned
    /// upload-pack RPC result. Proves the smart-HTTP pack-fetch POST is driven by
    /// the injected client, not a crate-constructed ureq one.
    struct PostRecorder {
        result_content_type: String,
        post_calls: std::sync::atomic::AtomicUsize,
    }

    impl HttpClient for PostRecorder {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
            Err(GitError::Command(
                "recording client received an unexpected GET".into(),
            ))
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<HttpResponse> {
            self.post_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(HttpResponse {
                status: 200,
                content_type: Some(self.result_content_type.clone()),
                body: Box::new(std::io::Cursor::new(Vec::new())),
            })
        }
    }

    #[test]
    fn http_upload_pack_post_uses_injected_client() {
        let recorder = PostRecorder {
            result_content_type: sley_protocol::smart_http_rpc_result_content_type(
                GitService::UploadPack,
            )
            .expect("content type"),
            post_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let remote = parse_remote_url("http://example.invalid/repo.git").expect("url");
        let want = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("oid");
        let request = UploadPackRequest {
            wants: vec![want],
            capabilities: Vec::new(),
            shallow: Vec::new(),
            deepen: None,
            deepen_since: None,
            deepen_not: Vec::new(),
            filter: None,
        };
        let mut credentials = crate::NoCredentials;
        let response = http_upload_pack_post(
            &recorder,
            &remote,
            &request,
            Vec::new(),
            &mut credentials,
            None,
            1 << 20,
        )
        .expect("post via injected client should succeed");
        assert_eq!(response.status, 200);
        assert_eq!(
            recorder
                .post_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the injected client must own the pack-fetch POST dial"
        );
    }

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

    struct ScriptedFetchClient<'a> {
        responses: Mutex<VecDeque<Vec<u8>>>,
        requests: Mutex<Vec<Vec<u8>>>,
        cancel_after_post: Option<&'a sley_core::AtomicCancel>,
    }

    impl ScriptedFetchClient<'_> {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
                cancel_after_post: None,
            }
        }

        fn requests(&self) -> Vec<ProtocolV2FetchRequest> {
            self.requests
                .lock()
                .expect("requests")
                .iter()
                .map(|body| {
                    let command = read_protocol_v2_command_request(&mut body.as_slice())
                        .expect("fetch command");
                    ProtocolV2FetchRequest::from_command_request(ObjectFormat::Sha1, &command)
                        .expect("fetch request")
                })
                .collect()
        }
    }

    impl HttpClient for ScriptedFetchClient<'_> {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
            Err(GitError::Command(
                "scripted fetch client received an unexpected GET".into(),
            ))
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            self.requests.lock().expect("requests").push(body.to_vec());
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("scripted response");
            if let Some(cancel) = self.cancel_after_post {
                cancel.cancel();
            }
            Ok(HttpResponse {
                status: 200,
                content_type: None,
                body: Box::new(std::io::Cursor::new(response)),
            })
        }
    }

    fn fetch_response(sections: &[ProtocolV2FetchResponseSection], sideband_all: bool) -> Vec<u8> {
        let mut response = Vec::new();
        if sideband_all {
            write_protocol_v2_fetch_sideband_all_response(&mut response, sections)
                .expect("sideband-all fetch response");
        } else {
            write_protocol_v2_fetch_response(&mut response, sections).expect("fetch response");
        }
        response
    }

    fn test_negotiation_request<'a, C: HttpClient + ?Sized>(
        client: &'a C,
        remote: &'a RemoteUrl,
        wants: Vec<ObjectId>,
        haves: Vec<ObjectId>,
    ) -> HttpFetchPackRequest<'a, C> {
        HttpFetchPackRequest {
            client,
            git_dir: Path::new("/no/local/repository/exists"),
            format: ObjectFormat::Sha1,
            remote,
            wants,
            haves: Some(haves),
            shallow: Vec::new(),
            deepen: None,
            promisor: false,
            max_input_size: None,
            filter: None,
            deepen_since: None,
            deepen_not: Vec::new(),
            deepen_relative: false,
            git_protocol: Some("version=2"),
            post_buffer: usize::MAX,
            omit_haves: false,
        }
    }

    fn test_oid(value: usize) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, &format!("{value:040x}")).expect("test oid")
    }

    #[test]
    fn negotiation_only_have_exhaustion_keeps_doubling_and_terminal_done() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "fetch".into(),
                value: Some("shallow".into()),
            }],
        };
        let nak = || {
            fetch_response(
                &[ProtocolV2FetchResponseSection::Acknowledgments(vec![
                    ProtocolV2FetchAcknowledgment::Nak,
                ])],
                false,
            )
        };
        let client = ScriptedFetchClient::new(vec![
            nak(),
            nak(),
            nak(),
            fetch_response(
                &[ProtocolV2FetchResponseSection::Packfile(Vec::new())],
                false,
            ),
        ]);
        let remote = parse_remote_url("http://example.invalid/repo.git").expect("remote");
        let haves = (100..133).map(test_oid).collect();
        let mut credentials = crate::NoCredentials;
        let response = negotiate_fetch_pack_via_http_protocol_v2(
            test_negotiation_request(&client, &remote, vec![test_oid(1)], haves),
            &handshake,
            &mut credentials,
            CancelFlag::never(),
        )
        .expect("negotiation");
        assert!(response.has_packfile);

        let requests = client.requests();
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.haves.len(), request.done))
                .collect::<Vec<_>>(),
            vec![(16, false), (32, false), (33, false), (33, true)]
        );
        assert!(
            requests.iter().all(|request| request.thin_pack),
            "every negotiation round must request thin-pack"
        );
    }

    #[test]
    fn negotiation_only_ack_ready_parses_sideband_all_shallow_info() {
        let handshake = sample_v2_fetch_handshake();
        let shallow = test_oid(90);
        let client = ScriptedFetchClient::new(vec![fetch_response(
            &[
                ProtocolV2FetchResponseSection::Acknowledgments(vec![
                    ProtocolV2FetchAcknowledgment::Ack(test_oid(10)),
                    ProtocolV2FetchAcknowledgment::Ready,
                ]),
                ProtocolV2FetchResponseSection::ShallowInfo(vec![
                    ProtocolV2FetchShallowInfo::Shallow(shallow.clone()),
                ]),
                ProtocolV2FetchResponseSection::Packfile(Vec::new()),
            ],
            true,
        )]);
        let remote = parse_remote_url("http://example.invalid/repo.git").expect("remote");
        let mut credentials = crate::NoCredentials;
        let mut response = negotiate_fetch_pack_via_http_protocol_v2(
            test_negotiation_request(
                &client,
                &remote,
                vec![test_oid(1)],
                (10..27).map(test_oid).collect(),
            ),
            &handshake,
            &mut credentials,
            CancelFlag::never(),
        )
        .expect("negotiation");
        assert_eq!(
            response.shallow_info,
            vec![ProtocolV2FetchShallowInfo::Shallow(shallow)]
        );
        assert!(response.has_packfile);
        let mut pack = Vec::new();
        StreamingSidebandReader::new(&mut response.body, |_: &[u8]| {})
            .read_to_end(&mut pack)
            .expect("empty packfile sideband");
        assert!(pack.is_empty());
        assert_eq!(
            client
                .requests()
                .iter()
                .map(|request| (request.haves.len(), request.done))
                .collect::<Vec<_>>(),
            vec![(16, false)]
        );
    }

    #[test]
    fn negotiation_only_empty_haves_sends_done_immediately() {
        let handshake = sample_v2_fetch_handshake();
        let client = ScriptedFetchClient::new(vec![fetch_response(
            &[ProtocolV2FetchResponseSection::Packfile(Vec::new())],
            true,
        )]);
        let remote = parse_remote_url("http://example.invalid/repo.git").expect("remote");
        let mut credentials = crate::NoCredentials;
        let response = negotiate_fetch_pack_via_http_protocol_v2(
            test_negotiation_request(&client, &remote, vec![test_oid(1)], Vec::new()),
            &handshake,
            &mut credentials,
            CancelFlag::never(),
        )
        .expect("negotiation");
        assert!(response.has_packfile);
        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].haves.is_empty());
        assert!(requests[0].done);
    }

    #[test]
    fn negotiation_only_polls_cancellation_between_rounds() {
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: vec![Capability {
                name: "fetch".into(),
                value: Some("shallow".into()),
            }],
        };
        let cancel = sley_core::AtomicCancel::new();
        let mut client = ScriptedFetchClient::new(vec![fetch_response(
            &[ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak,
            ])],
            false,
        )]);
        client.cancel_after_post = Some(&cancel);
        let remote = parse_remote_url("http://example.invalid/repo.git").expect("remote");
        let mut credentials = crate::NoCredentials;
        let error = match negotiate_fetch_pack_via_http_protocol_v2(
            test_negotiation_request(
                &client,
                &remote,
                vec![test_oid(1)],
                (100..117).map(test_oid).collect(),
            ),
            &handshake,
            &mut credentials,
            CancelFlag::new(&cancel),
        ) {
            Ok(_) => panic!("expected cancellation between negotiation rounds"),
            Err(error) => error,
        };
        assert!(matches!(error, GitError::Cancelled));
        assert_eq!(client.requests().len(), 1);
    }

    struct GitHttpBackendClient {
        executable: std::path::PathBuf,
        project_root: std::path::PathBuf,
        posted: Mutex<Vec<Vec<u8>>>,
    }

    impl GitHttpBackendClient {
        fn request(
            &self,
            method: &str,
            url: &str,
            content_type: Option<&str>,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            let target = url
                .strip_prefix("http://example.test")
                .ok_or_else(|| GitError::InvalidFormat(format!("unexpected test URL {url}")))?;
            let (path_info, query_string) = target.split_once('?').unwrap_or((target, ""));
            let mut command = std::process::Command::new(&self.executable);
            command
                .env("GIT_PROJECT_ROOT", &self.project_root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("REQUEST_METHOD", method)
                .env("PATH_INFO", path_info)
                .env("QUERY_STRING", query_string)
                .env("CONTENT_LENGTH", body.len().to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if let Some(content_type) = content_type {
                command.env("CONTENT_TYPE", content_type);
            }
            if let Some((_, protocol)) = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Git-Protocol"))
            {
                command.env("HTTP_GIT_PROTOCOL", protocol);
            }
            let mut child = command
                .spawn()
                .map_err(|error| GitError::Command(format!("start git-http-backend: {error}")))?;
            {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .ok_or_else(|| GitError::Command("git-http-backend stdin unavailable".into()))?
                    .write_all(body)?;
            }
            let output = child.wait_with_output().map_err(|error| {
                GitError::Command(format!("wait for git-http-backend: {error}"))
            })?;
            if !output.status.success() {
                return Err(GitError::Command(format!(
                    "git-http-backend failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            let (header, response_body) = split_cgi_response(&output.stdout)?;
            let mut status = 200;
            let mut response_content_type = None;
            for line in String::from_utf8_lossy(header).lines() {
                let Some((name, value)) = line.trim_end_matches('\r').split_once(':') else {
                    continue;
                };
                if name.eq_ignore_ascii_case("Status") {
                    status = value
                        .trim()
                        .split_once(' ')
                        .map(|(code, _)| code)
                        .unwrap_or(value.trim())
                        .parse()
                        .map_err(|error| {
                            GitError::InvalidFormat(format!(
                                "invalid git-http-backend status: {error}"
                            ))
                        })?;
                } else if name.eq_ignore_ascii_case("Content-Type") {
                    response_content_type = Some(value.trim().to_string());
                }
            }
            Ok(HttpResponse {
                status,
                content_type: response_content_type,
                body: Box::new(std::io::Cursor::new(response_body.to_vec())),
            })
        }

        fn take_posted_fetches(&self) -> Vec<ProtocolV2FetchRequest> {
            std::mem::take(&mut *self.posted.lock().expect("posted requests"))
                .into_iter()
                .filter_map(|body| {
                    let command = read_protocol_v2_command_request(&mut body.as_slice())
                        .expect("real fetch command");
                    if command.command == "fetch" {
                        Some(
                            ProtocolV2FetchRequest::from_command_request(
                                ObjectFormat::Sha1,
                                &command,
                            )
                            .expect("real fetch request"),
                        )
                    } else {
                        None
                    }
                })
                .collect()
        }
    }

    impl HttpClient for GitHttpBackendClient {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
            self.request("GET", url, None, headers, &[])
        }

        fn post(
            &self,
            url: &str,
            content_type: &str,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            self.posted
                .lock()
                .expect("posted requests")
                .push(body.to_vec());
            self.request("POST", url, Some(content_type), headers, body)
        }
    }

    fn split_cgi_response(output: &[u8]) -> Result<(&[u8], &[u8])> {
        if let Some(offset) = output.windows(4).position(|window| window == b"\r\n\r\n") {
            return Ok((&output[..offset], &output[offset + 4..]));
        }
        if let Some(offset) = output.windows(2).position(|window| window == b"\n\n") {
            return Ok((&output[..offset], &output[offset + 2..]));
        }
        Err(GitError::InvalidFormat(
            "git-http-backend response has no CGI header terminator".into(),
        ))
    }

    fn git_http_backend_executable() -> Option<std::path::PathBuf> {
        let output = std::process::Command::new("git")
            .arg("--exec-path")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let exec_path = std::path::PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
        ["git-http-backend", "git-http-backend.exe"]
            .into_iter()
            .map(|name| exec_path.join(name))
            .find(|path| path.is_file())
    }

    struct RealFetchRepository {
        root: std::path::PathBuf,
        commits: Vec<ObjectId>,
    }

    impl Drop for RealFetchRepository {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn git_command(cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn real_fetch_repository(commit_count: usize) -> RealFetchRepository {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sley-http-negotiation-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("test root");
        git_command(&root, &["init", "--quiet", "work"]);
        let work = root.join("work");
        git_command(&work, &["config", "user.name", "Sley Test"]);
        git_command(&work, &["config", "user.email", "sley@example.invalid"]);
        let mut commits = Vec::with_capacity(commit_count);
        for index in 0..commit_count {
            std::fs::write(work.join("history.txt"), format!("version {index}\n"))
                .expect("history file");
            git_command(&work, &["add", "history.txt"]);
            git_command(
                &work,
                &["commit", "--quiet", "-m", &format!("commit {index}")],
            );
            let oid =
                String::from_utf8(git_command(&work, &["rev-parse", "HEAD"])).expect("commit oid");
            commits.push(
                ObjectId::from_hex(ObjectFormat::Sha1, oid.trim()).expect("parsed commit oid"),
            );
        }
        git_command(&root, &["clone", "--quiet", "--bare", "work", "repo.git"]);
        RealFetchRepository { root, commits }
    }

    fn negotiated_pack_object_count(mut response: NegotiatedPackResponse) -> u32 {
        assert!(
            response.has_packfile,
            "server must return a packfile section"
        );
        let mut pack = Vec::new();
        StreamingSidebandReader::new(&mut response.body, |_: &[u8]| {})
            .read_to_end(&mut pack)
            .expect("read negotiated pack");
        assert!(pack.len() >= 12, "pack header is truncated");
        assert_eq!(&pack[..4], b"PACK");
        u32::from_be_bytes(pack[8..12].try_into().expect("pack object count"))
    }

    #[test]
    fn negotiation_only_real_incremental_fetch_is_delta_sized_and_multi_round() {
        let Some(executable) = git_http_backend_executable() else {
            eprintln!("skipping real HTTP negotiation test: git-http-backend unavailable");
            return;
        };
        let repository = real_fetch_repository(40);
        let client = GitHttpBackendClient {
            executable,
            project_root: repository.root.clone(),
            posted: Mutex::new(Vec::new()),
        };
        let remote = parse_remote_url("http://example.test/repo.git").expect("remote");
        let mut credentials = crate::NoCredentials;
        let discovered = http_service_advertisements(
            &client,
            &remote,
            ObjectFormat::Sha1,
            GitService::UploadPack,
            &mut credentials,
            None,
        )
        .expect("protocol v2 discovery");
        assert_eq!(discovered.set.protocol, ProtocolVersion::V2);
        let handshake = discovered.handshake.as_ref().expect("v2 handshake");
        let _ = client.take_posted_fetches();
        let tip = repository.commits.last().expect("tip").clone();
        let delta_base = repository.commits[37].clone();

        let run_fetch = |haves: Vec<ObjectId>| {
            let mut credentials = crate::NoCredentials;
            negotiate_fetch_pack_via_http_protocol_v2(
                test_negotiation_request(&client, &remote, vec![tip.clone()], haves),
                handshake,
                &mut credentials,
                CancelFlag::never(),
            )
            .expect("real fetch negotiation")
        };

        let full_objects = negotiated_pack_object_count(run_fetch(Vec::new()));
        let full_rounds = client.take_posted_fetches();
        let delta_objects = negotiated_pack_object_count(run_fetch(vec![delta_base.clone()]));
        let delta_rounds = client.take_posted_fetches();
        let already_present_objects = negotiated_pack_object_count(run_fetch(vec![tip.clone()]));
        let already_present_rounds = client.take_posted_fetches();

        let mut delayed_have = (1_000..1_016).map(test_oid).collect::<Vec<_>>();
        delayed_have.push(delta_base);
        let delayed_objects = negotiated_pack_object_count(run_fetch(delayed_have));
        let delayed_rounds = client.take_posted_fetches();

        assert_eq!(full_rounds.len(), 1);
        assert_eq!(delta_rounds.len(), 1);
        assert_eq!(already_present_rounds.len(), 1);
        assert_eq!(
            delayed_rounds
                .iter()
                .map(|request| (request.haves.len(), request.done))
                .collect::<Vec<_>>(),
            vec![(16, false), (17, false)]
        );
        assert!(
            full_rounds
                .iter()
                .chain(&delta_rounds)
                .chain(&already_present_rounds)
                .chain(&delayed_rounds)
                .all(|request| request.thin_pack),
            "real HTTP fetches must retain thin-pack"
        );
        assert_eq!(already_present_objects, 0);
        assert!(
            full_objects >= 100,
            "40 commits should produce a history-sized pack, got {full_objects} objects"
        );
        assert!(
            delta_objects <= 9,
            "two commits should produce a delta-sized pack, got {delta_objects} objects"
        );
        assert!(
            delta_objects * 10 < full_objects,
            "delta pack ({delta_objects}) must be proportional to the delta, not full history ({full_objects})"
        );
        assert_eq!(delayed_objects, delta_objects);
        eprintln!(
            "real negotiation evidence: history_commits=40 full_objects={full_objects} \
             delta_objects={delta_objects} already_present_objects={already_present_objects} \
             immediate_have_rounds={} delayed_have_rounds={}",
            delta_rounds.len(),
            delayed_rounds.len()
        );
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
            Vec::new(),
            vec![have.clone()],
            vec![shallow.clone()],
            Some(3),
            None,
            Vec::new(),
            false,
            None,
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
                thin_pack: true,
                include_tag: true,
                ofs_delta: true,
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
            Vec::new(),
            vec![shallow.clone()],
            Some(1),
            None,
            Vec::new(),
            false,
            None,
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
            parsed.first(),
            Some(&ProtocolV2FetchResponseSection::ShallowInfo(vec![
                ProtocolV2FetchShallowInfo::Shallow(
                    ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    )
                    .expect("test operation should succeed")
                )
            ]))
        );
        assert!(!request_body.is_empty());
    }
}

#[cfg(test)]
mod bounded_read_tests {
    use super::*;

    fn limits_of(bytes: u64) -> TransportLimits {
        TransportLimits {
            max_ref_advertisement_bytes: bytes,
            ..TransportLimits::default()
        }
    }

    /// sley#163: the service-discovery response is buffered whole, so it needs
    /// a ceiling. Without one this call never returns on an endless reader.
    #[test]
    fn service_discovery_response_refuses_an_endless_reader() {
        let mut endless = std::io::repeat(b'x');
        let error = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut endless,
            limits_of(64 * 1024),
        )
        .expect_err("an endless advertisement must be refused");
        assert!(
            error.to_string().contains("exceeds the configured ceiling"),
            "unexpected error: {error}"
        );
    }

    /// The ceiling moves, and it still binds where it now sits. A raised
    /// limit is a different wall, not the absence of one.
    #[test]
    fn a_raised_ceiling_admits_more_and_still_refuses_an_endless_reader() {
        // Below the old ceiling, above the new one: proof the wall moved.
        let mut endless = std::io::repeat(b'x');
        let error = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut endless,
            limits_of(4 * 1024 * 1024),
        )
        .expect_err("an endless advertisement must be refused at the raised ceiling too");
        assert!(
            error.to_string().contains("4194304 bytes"),
            "the raised ceiling must be the one reported: {error}"
        );

        // A body that the default ceiling would refuse is admitted once the
        // ceiling is raised past it -- it fails on parsing, not on size.
        let body = vec![b'x'; 128 * 1024];
        let error = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut body.as_slice(),
            limits_of(64 * 1024),
        )
        .expect_err("128 KiB must not fit under a 64 KiB ceiling");
        assert!(
            error.to_string().contains("exceeds the configured ceiling"),
            "unexpected error: {error}"
        );
        let error = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut body.as_slice(),
            limits_of(256 * 1024),
        )
        .expect_err("the body is not a valid advertisement");
        assert!(
            !error.to_string().contains("exceeds the configured ceiling"),
            "the raised ceiling must admit the body: {error}"
        );
    }

    /// A limit whose only remedy is "rebuild" is not a remedy. The error has
    /// to name what was seen, what the ceiling was, and what to do -- and the
    /// real remedy here is the protocol, not a larger buffer.
    #[test]
    fn the_over_ceiling_error_names_the_size_the_limit_and_the_remedy() {
        let mut endless = std::io::repeat(b'x');
        let error = read_http_service_discovery_response_with_limits(
            ObjectFormat::Sha1,
            &mut endless,
            limits_of(64 * 1024),
        )
        .expect_err("an endless advertisement must be refused")
        .to_string();
        // what was read, and the ceiling it was measured against
        assert!(error.contains("stopped at 65537 bytes"), "{error}");
        assert!(error.contains("ceiling of 65536 bytes"), "{error}");
        // the remedy, in the order it should be tried
        assert!(error.contains("ls-refs"), "{error}");
        assert!(error.contains("ref-prefix"), "{error}");
        assert!(
            error.contains("git config sley.maxRefAdvertisementBytes"),
            "{error}"
        );
        // and never "edit this constant"
        assert!(!error.contains("recompile"), "{error}");
    }

    /// The default ceilings are what an unconfigured caller has always had.
    #[test]
    fn no_configuration_leaves_the_documented_defaults_in_place() {
        assert_eq!(
            transport_limits_from_config(None),
            TransportLimits::default()
        );
        let empty = GitConfig::parse(b"[core]\n\tbare = false\n").expect("config");
        assert_eq!(
            transport_limits_from_config(Some(&empty)),
            TransportLimits::default()
        );
        assert_eq!(
            TransportLimits::default().max_ref_advertisement_bytes,
            128 * 1024 * 1024
        );
        assert_eq!(TransportLimits::default().admitted_refs(), 512 * 1024);
    }

    /// Config raises the ceiling in the units an operator already uses, and
    /// the derived body deadline moves with it.
    #[test]
    fn git_config_raises_the_ceilings_and_the_derived_deadline() {
        let config = GitConfig::parse(
            b"[sley]\n\tmaxRefAdvertisementBytes = 512m\n\tmaxPackfileResponseBytes = 8g\n",
        )
        .expect("config");
        let limits = transport_limits_from_config(Some(&config));
        assert_eq!(limits.max_ref_advertisement_bytes, 512 * 1024 * 1024);
        assert_eq!(limits.max_packfile_response_bytes, 8 * 1024 * 1024 * 1024);
        // 2Mi refs at the worst-case 256 bytes each -- past the million-ref
        // shape a ref-per-patchset review system reaches.
        assert_eq!(limits.admitted_refs(), 2 * 1024 * 1024);
        assert!(
            limits.body_transfer_timeout() > TransportLimits::default().body_transfer_timeout()
        );
    }

    /// Configuration moves a ceiling; it never removes one. Whatever config
    /// asks for, the result is finite and the deadline is still a deadline.
    #[test]
    fn git_config_cannot_remove_a_ceiling() {
        let config = GitConfig::parse(
            b"[sley]\n\tmaxRefAdvertisementBytes = 0\n\tmaxPackfileResponseBytes = 1024g\n\tminTransferBytesPerSec = 0\n",
        )
        .expect("config");
        let limits = transport_limits_from_config(Some(&config));
        // 0 reads as "unset", not as "refuse everything"
        assert_eq!(
            limits.max_ref_advertisement_bytes,
            TransportLimits::default().max_ref_advertisement_bytes
        );
        assert_eq!(
            limits.min_transfer_bytes_per_sec,
            TransportLimits::default().min_transfer_bytes_per_sec
        );
        // and an absurd ceiling is clamped rather than trusted
        assert_eq!(
            limits.max_packfile_response_bytes,
            sley_protocol::MAX_CONFIGURABLE_RESPONSE_BYTES
        );
        assert!(limits.body_transfer_timeout() <= sley_protocol::MAX_BODY_TRANSFER_TIMEOUT);
        assert!(limits.body_transfer_timeout() > std::time::Duration::ZERO);

        // an unparseable value is not a way to opt out either
        let garbage =
            GitConfig::parse(b"[sley]\n\tmaxRefAdvertisementBytes = enormous\n").expect("config");
        assert_eq!(
            transport_limits_from_config(Some(&garbage)),
            TransportLimits::default()
        );
    }
}
