//! Protocol v2 `bundle-uri` discovery and bundle prefetch for clone.
//!
//! Mirrors upstream `bundle-uri.c` / `connect.c::get_remote_bundle_uri` enough for
//! clone auto-discovery: issue `command=bundle-uri`, parse `bundle.*=…` lines into a
//! [`BundleUriList`], download HTTP(S) bundles via `git-remote-https`, and install
//! them into the destination repository before the main clone fetch.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sley_config::GitConfig;
use sley_core::{Capability, GitError, ObjectFormat, Result, UPSTREAM_GIT_COMPAT_VERSION};
use sley_formats::Bundle;
use sley_odb::{FileObjectDatabase, install_bundle_pack};
use sley_protocol::{
    GitService, PktLineFrame, ProtocolV2CommandRequest, TransportHandshake,
    read_protocol_v2_stateless_rpc_payload_frames, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type, write_protocol_v2_command_request,
};
use sley_transport::{
    HttpClient, RemoteUrl, http_smart_rpc_url, parse_remote_url,
};

use crate::CredentialProvider;
use crate::http::{
    http_check_status, http_git_protocol_header_value, http_request_headers, http_send_with_auth,
    http_validate_content_type,
};

#[derive(Debug, Clone, Default)]
pub struct BundleUriEntry {
    pub id: String,
    pub uri: Option<String>,
    pub creation_token: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BundleUriList {
    pub version: i32,
    pub mode_all: bool,
    pub creation_token_heuristic: bool,
    pub base_uri: String,
    pub bundles: BTreeMap<String, BundleUriEntry>,
}

pub fn handshake_advertises_bundle_uri(handshake: &TransportHandshake) -> bool {
    handshake
        .capabilities
        .iter()
        .any(|cap| cap.name == "bundle-uri")
}

pub fn transfer_bundle_uri_enabled(config: &GitConfig) -> bool {
    config
        .get_bool("transfer", None, "bundleuri")
        .unwrap_or(false)
}

pub fn parse_bundle_uri_line(list: &mut BundleUriList, line: &str) -> Result<()> {
    if line.is_empty() {
        return Err(GitError::InvalidFormat("bundle-uri: got an empty line".into()));
    }
    let Some((key, value)) = line.split_once('=') else {
        return Err(GitError::InvalidFormat(
            "bundle-uri: line is not of the form 'key=value'".into(),
        ));
    };
    if key.is_empty() || value.is_empty() {
        return Err(GitError::InvalidFormat(
            "bundle-uri: line has empty key or value".into(),
        ));
    }
    if key == "bundle.version" {
        list.version = value.parse().map_err(|_| {
            GitError::InvalidFormat("bundle-uri: invalid bundle.version".into())
        })?;
        if list.version != 1 {
            return Err(GitError::InvalidFormat("bundle-uri: unsupported bundle.version".into()));
        }
        return Ok(());
    }
    if key == "bundle.mode" {
        list.mode_all = value == "all";
        return Ok(());
    }
    if key == "bundle.heuristic" {
        list.creation_token_heuristic = value.eq_ignore_ascii_case("creationToken");
        return Ok(());
    }
    let Some(rest) = key.strip_prefix("bundle.") else {
        return Ok(());
    };
    let Some((id, subkey)) = rest.split_once('.') else {
        return Ok(());
    };
    let entry = list.bundles.entry(id.to_string()).or_insert_with(|| BundleUriEntry {
        id: id.to_string(),
        ..BundleUriEntry::default()
    });
    if subkey == "uri" {
        if entry.uri.is_some() {
            return Err(GitError::InvalidFormat(format!(
                "bundle-uri: duplicate uri for bundle \"{id}\""
            )));
        }
        entry.uri = Some(relative_bundle_uri(&list.base_uri, value));
        return Ok(());
    }
    if subkey.eq_ignore_ascii_case("creationtoken") {
        entry.creation_token = value.parse().unwrap_or(0);
    }
    Ok(())
}

fn relative_bundle_uri(base_uri: &str, value: &str) -> String {
    if value.contains("://") {
        return value.to_string();
    }
    let base = base_uri.trim_end_matches('/');
    let value = value.trim_start_matches('/');
    format!("{base}/{value}")
}

pub fn http_remote_bundle_uri_list(
    client: &dyn HttpClient,
    remote: &RemoteUrl,
    handshake: &TransportHandshake,
    credentials: &mut dyn CredentialProvider,
    config: Option<&GitConfig>,
) -> Result<BundleUriList> {
    let git_protocol = http_git_protocol_header_value(config)?;
    if !handshake_advertises_bundle_uri(handshake) {
        return Ok(BundleUriList::default());
    }
    let mut command = ProtocolV2CommandRequest::new("bundle-uri")?;
    command.capabilities.push(Capability {
        name: "agent".into(),
        value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
    });
    let url = http_smart_rpc_url(remote, GitService::UploadPack)?;
    let mut body = Vec::new();
    write_protocol_v2_command_request(&mut body, &command)?;
    let content_type = smart_http_rpc_request_content_type(GitService::UploadPack)?;
    let mut response = http_send_with_auth(remote, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &http_request_headers(auth, git_protocol.as_deref()),
            &body,
        )
    })?;
    http_check_status(&response, &url)?;
    http_validate_content_type(
        &response,
        &smart_http_rpc_result_content_type(GitService::UploadPack)?,
    )?;
    let mut list = BundleUriList {
        base_uri: remote_base_uri(remote),
        ..BundleUriList::default()
    };
    let frames = read_protocol_v2_stateless_rpc_payload_frames(&mut response.body)?;
    for frame in frames {
        let PktLineFrame::Data(payload) = frame else {
            break;
        };
        let line = std::str::from_utf8(&payload)
            .map_err(|_| GitError::InvalidFormat("bundle-uri response is not UTF-8".into()))?
            .trim_end_matches('\n');
        parse_bundle_uri_line(&mut list, line)?;
    }
    Ok(list)
}

fn remote_base_uri(remote: &RemoteUrl) -> String {
    let scheme = match remote.transport {
        sley_transport::RemoteTransport::Http => "http",
        sley_transport::RemoteTransport::Https => "https",
        _ => "http",
    };
    let host = remote.host.as_deref().unwrap_or("localhost");
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match remote.port {
        Some(port) => format!("{scheme}://{host}:{port}{}", remote.path.trim_end_matches('/')),
        None => format!("{scheme}://{host}{}", remote.path.trim_end_matches('/')),
    }
}

pub fn bundle_uri_fetch_order(list: &BundleUriList) -> Vec<String> {
    let mut entries: Vec<_> = list.bundles.values().collect();
    if list.creation_token_heuristic {
        entries.sort_by(|left, right| right.creation_token.cmp(&left.creation_token));
    }
    entries
        .into_iter()
        .filter_map(|entry| entry.uri.clone())
        .collect()
}

pub fn prefetch_advertised_bundle_uris(
    git_dir: &Path,
    format: ObjectFormat,
    list: &BundleUriList,
) -> Result<()> {
    if list.bundles.is_empty() {
        return Ok(());
    }
    for uri in bundle_uri_fetch_order(list) {
        if uri.is_empty() {
            continue;
        }
        let temp = download_bundle_uri_to_temp(&uri)?;
        let bytes = fs::read(&temp)?;
        let _ = fs::remove_file(&temp);
        let bundle = Bundle::parse(&bytes, format)?;
        let reader = FileObjectDatabase::from_git_dir(git_dir, format);
        install_bundle_pack(&bundle, &reader, &reader)?;
    }
    Ok(())
}

fn download_bundle_uri_to_temp(uri: &str) -> Result<PathBuf> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return download_https_bundle_uri(uri);
    }
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    Ok(PathBuf::from(path))
}

fn download_https_bundle_uri(uri: &str) -> Result<PathBuf> {
    if uri.contains(' ') || uri.contains('\n') {
        return Err(GitError::InvalidFormat(format!(
            "bundle-uri: URI is malformed: '{uri}'"
        )));
    }
    let helper = locate_git_remote_https()?;
    let temp = std::env::temp_dir().join(format!(
        "sley-bundle-uri-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let argv = vec!["git-remote-https".to_string(), uri.to_string()];
    sley_core::trace2::child_start("??", &argv);
    let mut child = Command::new(&helper)
        .arg(&uri)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| GitError::Command(format!("failed to spawn git-remote-https: {err}")))?;
    let mut stdin = child.stdin.take().expect("git-remote-https stdin");
    writeln!(stdin, "get {uri} {path}", path = temp.display())
        .map_err(|err| GitError::Io(err.to_string()))?;
    drop(stdin);
    let mut stdout = child.stdout.take().expect("git-remote-https stdout");
    let mut line = String::new();
    let mut found_get = false;
    let mut reader = std::io::BufReader::new(&mut stdout);
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|err| GitError::Io(err.to_string()))?;
        if read == 0 {
            break;
        }
        if line.trim() == "done" {
            break;
        }
        if line.starts_with("ack ") {
            found_get = true;
        }
    }
    let status = child
        .wait()
        .map_err(|err| GitError::Command(format!("git-remote-https wait failed: {err}")))?;
    if !status.success() || !found_get {
        let _ = fs::remove_file(&temp);
        return Err(GitError::Command(format!(
            "failed to download bundle from URI '{uri}'"
        )));
    }
    Ok(temp)
}

fn locate_git_remote_https() -> Result<PathBuf> {
    for exec_path in git_exec_path_candidates() {
        let candidate = exec_path.join("git-remote-https");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("git-remote-https");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(GitError::Command(
        "git-remote-https is not available on PATH or in GIT_EXEC_PATH".into(),
    ))
}

fn git_exec_path_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exec_path) = std::env::var("GIT_EXEC_PATH") {
        paths.push(PathBuf::from(exec_path));
    }
    for var in ["SLEY_TEST_GIT", "GIT_TEST_GIT"] {
        if let Ok(program) = std::env::var(var)
            && !program.is_empty()
            && let Some(exec_path) = git_exec_path_from_program(&program)
        {
            paths.push(exec_path);
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("git");
            if candidate.is_file() {
                if let Some(exec_path) = git_exec_path_from_program(candidate.to_string_lossy().as_ref())
                {
                    paths.push(exec_path);
                }
            }
        }
    }
    for candidate in [
        "/opt/homebrew/opt/git/libexec/git-core",
        "/usr/local/libexec/git-core",
        "/usr/lib/git-core",
        "/usr/libexec/git-core",
    ] {
        paths.push(PathBuf::from(candidate));
    }
    paths
}

fn git_exec_path_from_program(program: &str) -> Option<PathBuf> {
    let output = Command::new(program).arg("--exec-path").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

pub fn remote_url_from_bundle_uri(uri: &str) -> Result<RemoteUrl> {
    parse_remote_url(uri)
}