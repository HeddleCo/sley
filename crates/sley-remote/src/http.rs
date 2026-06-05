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

use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_fetch::{install_upload_pack_raw_promisor_response, install_upload_pack_raw_response};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    parse_upload_pack_features, read_upload_pack_raw_packfile_response,
    read_upload_pack_shallow_info_and_raw_packfile_response, smart_http_advertisement_content_type,
    smart_http_rpc_request_content_type, smart_http_rpc_result_content_type,
    write_upload_pack_negotiation_request, write_upload_pack_request, GitService,
    ProtocolV2FetchShallowInfo, RefAdvertisement, RefAdvertisementSet, UploadPackFeatures,
    UploadPackNegotiationRequest, UploadPackRawPackfileResponse, UploadPackRequest,
};
use sley_transport::{
    git_credential_basic_authorization, http_smart_info_refs_url, http_smart_rpc_url,
    parse_remote_url, read_service_discovery_response, HttpClient, HttpResponse, RemoteTransport,
    RemoteUrl, ServiceDiscoveryPayload, UreqHttpClient,
};

use crate::credentials::{credential_request_for_url, http_url_credential};
use crate::CredentialProvider;

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

/// Parse a smart-HTTP info/refs body into a ref advertisement set, rejecting the
/// (currently unsupported) protocol v2 advertisement form.
pub fn http_advertised_refs(
    format: ObjectFormat,
    mut response: HttpResponse,
) -> Result<RefAdvertisementSet> {
    let discovery = read_service_discovery_response(format, &mut response.body)?;
    match discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(set),
        ServiceDiscoveryPayload::ProtocolV2(_) => Err(GitError::Unsupported(
            "protocol v2 advertisements over HTTP are not supported yet".into(),
        )),
    }
}

/// Fetch and parse the ref advertisements for `service` from the smart-HTTP
/// info/refs endpoint, authenticating and validating status + content type.
pub fn http_service_advertisements(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    service: GitService,
    credentials: &mut dyn CredentialProvider,
) -> Result<RefAdvertisementSet> {
    let url = http_smart_info_refs_url(remote, service)?;
    let response = http_send_with_auth(remote, credentials, |auth| {
        client.get(&url, &http_authorization_headers(auth))
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(&response, &smart_http_advertisement_content_type(service)?)?;
    http_advertised_refs(format, response)
}

/// The upload-pack ref advertisements and parsed features for `remote`.
pub fn http_upload_pack_advertisements(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    format: ObjectFormat,
    credentials: &mut dyn CredentialProvider,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    let set =
        http_service_advertisements(client, remote, format, GitService::UploadPack, credentials)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok((set.refs, features))
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
            body.clone(),
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
#[allow(clippy::too_many_arguments)]
pub fn install_fetch_pack_via_http_upload_pack(
    client: &UreqHttpClient,
    git_dir: &Path,
    format: ObjectFormat,
    remote: &RemoteUrl,
    wants: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    promisor: bool,
    credentials: &mut dyn CredentialProvider,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    // A deepen request must always reach the server (the shallow boundary may move
    // even when every wanted object is already present), so only the plain fetch
    // takes the "everything is local already" shortcut.
    if deepen.is_none()
        && wants
            .iter()
            .map(|want| local_db.contains(want))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .all(|contains| contains)
    {
        return Ok(Vec::new());
    }
    let request = UploadPackRequest {
        wants,
        capabilities: shallow_request_capabilities(deepen),
        shallow,
        deepen,
        ..UploadPackRequest::default()
    };
    let haves = crate::local::local_have_oids(git_dir, format)?;
    let (shallow_info, response) = if deepen.is_some() {
        http_upload_pack_shallow_fetch_response(
            client,
            remote,
            format,
            request,
            haves,
            credentials,
        )?
    } else {
        let response =
            http_upload_pack_fetch_response(client, remote, format, request, haves, credentials)?;
        (Vec::new(), response)
    };
    if promisor {
        install_upload_pack_raw_promisor_response(&response, &local_db)?;
    } else {
        install_upload_pack_raw_response(&response, &local_db)?;
    }
    Ok(shallow_info)
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
