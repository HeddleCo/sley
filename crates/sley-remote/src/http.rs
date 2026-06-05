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

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_fetch::{install_upload_pack_raw_promisor_response, install_upload_pack_raw_response};
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    parse_upload_pack_features, read_upload_pack_raw_packfile_response,
    smart_http_advertisement_content_type, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type, write_upload_pack_negotiation_request,
    write_upload_pack_request, GitService, RefAdvertisement, RefAdvertisementSet,
    UploadPackFeatures, UploadPackNegotiationRequest, UploadPackRawPackfileResponse,
    UploadPackRequest,
};
use sley_transport::{
    git_credential_basic_authorization, http_smart_info_refs_url, http_smart_rpc_url,
    parse_remote_url, read_service_discovery_response, HttpClient, HttpResponse, RemoteTransport,
    RemoteUrl, ServiceDiscoveryPayload, UreqHttpClient,
};

use crate::credentials::{
    credential_fill, credential_request_for_url, credential_store, http_url_credential,
};

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

/// Perform an HTTP request, retrying once with credential-helper-supplied
/// authentication if the first attempt returns 401. `perform` is invoked with an
/// optional `Authorization` header value and must be idempotent (it may run twice).
pub fn http_send_with_auth(
    remote: &RemoteUrl,
    config: Option<&GitConfig>,
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
    let Some(filled) = credential_fill(config, request)? else {
        return Ok(response);
    };
    let Some(header) = git_credential_basic_authorization(&filled)? else {
        return Ok(response);
    };
    let retry = perform(Some(&header))?;
    credential_store(config, &filled, retry.status != 401);
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
    config: Option<&GitConfig>,
    format: ObjectFormat,
    service: GitService,
) -> Result<RefAdvertisementSet> {
    let url = http_smart_info_refs_url(remote, service)?;
    let response = http_send_with_auth(remote, config, |auth| {
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
    config: Option<&GitConfig>,
    format: ObjectFormat,
) -> Result<(Vec<RefAdvertisement>, UploadPackFeatures)> {
    let set = http_service_advertisements(client, remote, config, format, GitService::UploadPack)?;
    let features = set
        .refs
        .first()
        .map(|advertisement| parse_upload_pack_features(&advertisement.capabilities))
        .transpose()?
        .unwrap_or_default();
    Ok((set.refs, features))
}

/// Post an upload-pack RPC `request` + `haves` and read back the raw packfile
/// response, authenticating and validating status + content type.
pub fn http_upload_pack_fetch_response(
    client: &UreqHttpClient,
    remote: &RemoteUrl,
    config: Option<&GitConfig>,
    format: ObjectFormat,
    request: UploadPackRequest,
    haves: Vec<ObjectId>,
) -> Result<UploadPackRawPackfileResponse> {
    let url = http_smart_rpc_url(remote, GitService::UploadPack)?;
    let mut body = Vec::new();
    write_upload_pack_request(&mut body, Some(&request))?;
    write_upload_pack_negotiation_request(
        &mut body,
        &UploadPackNegotiationRequest { haves, done: true },
    )?;
    let content_type = smart_http_rpc_request_content_type(GitService::UploadPack)?;
    let mut response = http_send_with_auth(remote, config, |auth| {
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
    read_upload_pack_raw_packfile_response(format, &mut response.body)
}

/// Fetch `wants` from an HTTP upload-pack remote into the repository at `git_dir`,
/// installing the resulting pack. Objects already present locally are skipped;
/// `promisor` selects promisor-pack installation.
pub fn install_fetch_pack_via_http_upload_pack(
    client: &UreqHttpClient,
    git_dir: &Path,
    format: ObjectFormat,
    remote: &RemoteUrl,
    config: Option<&GitConfig>,
    wants: Vec<ObjectId>,
    promisor: bool,
) -> Result<()> {
    if wants.is_empty() {
        return Ok(());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    if wants
        .iter()
        .map(|want| local_db.contains(want))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .all(|contains| contains)
    {
        return Ok(());
    }
    let request = UploadPackRequest {
        wants,
        ..UploadPackRequest::default()
    };
    let haves = crate::local::local_have_oids(git_dir, format)?;
    let response = http_upload_pack_fetch_response(client, remote, config, format, request, haves)?;
    if promisor {
        install_upload_pack_raw_promisor_response(&response, &local_db)?;
    } else {
        install_upload_pack_raw_response(&response, &local_db)?;
    }
    Ok(())
}
