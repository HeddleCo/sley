//! Protocol v2 `bundle-uri` discovery and bundle prefetch for clone.
//!
//! Mirrors upstream `bundle-uri.c` / `connect.c::get_remote_bundle_uri` enough for
//! clone auto-discovery: issue `command=bundle-uri`, parse `bundle.*=…` lines into a
//! [`BundleUriList`], download HTTP(S) bundles through Sley's native transport,
//! and install them into the destination repository before the main clone fetch.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{Capability, GitError, ObjectFormat, Result, UPSTREAM_GIT_COMPAT_VERSION};
use sley_formats::Bundle;
use sley_odb::{FileObjectDatabase, install_bundle_pack, verify_bundle_prerequisites};
use sley_protocol::{
    GitService, PktLineFrame, ProtocolV2CommandRequest, TransportHandshake,
    read_protocol_v2_stateless_rpc_payload_frames, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type, write_protocol_v2_command_request,
};
use sley_refs::{FileRefStore, RefTarget, RefUpdate};
use sley_transport::{HttpClient, RemoteUrl, UreqHttpClient, http_smart_rpc_url, parse_remote_url};

use crate::CredentialProvider;
use crate::http::{
    http_check_status, http_git_protocol_header_value, http_request_headers, http_send_with_auth,
    http_validate_content_type,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BundleMode {
    /// `bundle.mode = none` (or unset): upstream warns but still fetches.
    #[default]
    None,
    /// `bundle.mode = all`: every bundle in the list is needed.
    All,
    /// `bundle.mode = any`: a single successfully-applied bundle suffices, so
    /// prefetch stops after the first bundle it manages to unbundle.
    Any,
}

#[derive(Debug, Clone, Default)]
pub struct BundleUriEntry {
    pub id: String,
    pub uri: Option<String>,
    pub creation_token: u64,
}

#[derive(Debug, Clone, Default)]
pub struct BundleUriList {
    pub version: i32,
    pub mode: BundleMode,
    pub creation_token_heuristic: bool,
    pub base_uri: String,
    pub bundles: BTreeMap<String, BundleUriEntry>,
}

/// Split a `bundle.<subsection>.<key>` variable using `parse_config_key`
/// semantics: the section (`bundle`) is matched case-sensitively, and the
/// subsection/key boundary is the LAST dot (a subsection may itself contain
/// dots). Returns `(subsection, key)` where `subsection` is `None` for a bare
/// `bundle.<key>` (e.g. `bundle.version`). Returns `None` when `var` does not
/// begin with `bundle.` (upstream `parse_config_key` returning -1).
fn parse_bundle_config_key(var: &str) -> Option<(Option<&str>, &str)> {
    let rest = var.strip_prefix("bundle")?;
    let rest = rest.strip_prefix('.')?;
    // `rest` is the text after "bundle."; the key runs from the last dot.
    match rest.rfind('.') {
        Some(dot) => Some((Some(&rest[..dot]), &rest[dot + 1..])),
        None => Some((None, rest)),
    }
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

/// Parse one `key=value` line from a protocol-v2 `bundle-uri` response into
/// `list`, mirroring upstream `bundle_uri_parse_line` / `bundle_list_update`.
///
/// The comparison is byte-exact (upstream uses `strcmp`, not case-insensitive
/// matching, and never trims whitespace): the key is split at the FIRST `=`, an
/// empty key or value is an error, and the `bundle.<subsection>.<key>` split
/// uses `parse_config_key` (subsection = up to the last dot). Unknown
/// `bundle.mode` values and non-`1` versions are errors; an unparseable
/// `creationtoken` is warned about (not silently zeroed) and otherwise ignored.
pub fn parse_bundle_uri_line(list: &mut BundleUriList, line: &str) -> Result<()> {
    if line.is_empty() {
        return Err(GitError::InvalidFormat(
            "bundle-uri: got an empty line".into(),
        ));
    }
    let Some(equals) = line.find('=') else {
        return Err(GitError::InvalidFormat(
            "bundle-uri: line is not of the form 'key=value'".into(),
        ));
    };
    let key = &line[..equals];
    let value = &line[equals + 1..];
    if key.is_empty() || value.is_empty() {
        return Err(GitError::InvalidFormat(
            "bundle-uri: line has empty key or value".into(),
        ));
    }
    let Some((subsection, subkey)) = parse_bundle_config_key(key) else {
        // parse_config_key returned -1: the key is not under the `bundle`
        // section. Upstream treats this as an error for the line.
        return Err(GitError::InvalidFormat(format!(
            "bundle-uri: line is not of the form 'key=value': {line}"
        )));
    };

    let Some(id) = subsection else {
        // Global `bundle.<key>` settings.
        match subkey {
            "version" => {
                let version: i32 = value.parse().map_err(|_| {
                    GitError::InvalidFormat("bundle-uri: invalid bundle.version".into())
                })?;
                if version != 1 {
                    return Err(GitError::InvalidFormat(
                        "bundle-uri: unsupported bundle.version".into(),
                    ));
                }
                list.version = version;
            }
            "mode" => {
                list.mode = match value {
                    "all" => BundleMode::All,
                    "any" => BundleMode::Any,
                    _ => {
                        return Err(GitError::InvalidFormat(format!(
                            "bundle-uri: unrecognized bundle.mode value '{value}'"
                        )));
                    }
                };
            }
            "heuristic"
                // Unknown heuristics are ignored (upstream loops the known
                // heuristics and returns 0 without matching).
                if value == "creationToken" => {
                    list.creation_token_heuristic = true;
                }
            // Any other global `bundle.<key>` is an unknown hint: ignore.
            _ => {}
        }
        return Ok(());
    };

    let entry = list
        .bundles
        .entry(id.to_string())
        .or_insert_with(|| BundleUriEntry {
            id: id.to_string(),
            ..BundleUriEntry::default()
        });
    match subkey {
        "uri" => {
            if entry.uri.is_some() {
                return Err(GitError::InvalidFormat(format!(
                    "bundle-uri: duplicate uri for bundle \"{id}\""
                )));
            }
            entry.uri = Some(relative_bundle_uri(&list.base_uri, value));
        }
        "creationtoken" => match value.parse() {
            Ok(token) => entry.creation_token = token,
            Err(_) => {
                eprintln!(
                    "warning: could not parse bundle list key creationToken with value '{value}'"
                );
            }
        },
        // Unknown per-bundle keys are hints for heuristics we do not implement.
        _ => {}
    }
    Ok(())
}

/// Resolve a bundle `uri` value against `base_uri`, mirroring upstream
/// `relative_url`: absolute URLs (containing `://`) and absolute filesystem
/// paths (leading `/`) pass through unchanged; only genuinely relative values
/// are joined onto the base.
fn relative_bundle_uri(base_uri: &str, value: &str) -> String {
    if value.contains("://") || value.starts_with('/') {
        return value.to_string();
    }
    let base = base_uri.trim_end_matches('/');
    let value = value.trim_start_matches("./");
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
        Some(port) => format!(
            "{scheme}://{host}:{port}{}",
            remote.path.trim_end_matches('/')
        ),
        None => format!("{scheme}://{host}{}", remote.path.trim_end_matches('/')),
    }
}

/// The order in which the advertised bundle URIs should be *downloaded*.
///
/// Upstream `fetch_bundles_by_token` downloads bundles newest-first (decreasing
/// creationToken) under the creationToken heuristic, so a client that is only a
/// little behind can stop after the first few bundles. Without the heuristic the
/// list order (here, the deterministic `bundle.<id>` ordering of the `BTreeMap`)
/// is used. This download order is what t5601's GIT_TRACE2 assertions check.
pub fn bundle_uri_fetch_order(list: &BundleUriList) -> Vec<String> {
    ordered_bundle_entries(list)
        .into_iter()
        .filter_map(|entry| entry.uri.clone())
        .collect()
}

fn ordered_bundle_entries(list: &BundleUriList) -> Vec<&BundleUriEntry> {
    let mut entries: Vec<_> = list.bundles.values().collect();
    if list.creation_token_heuristic {
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.creation_token));
    }
    entries
}

/// Download every advertised bundle (newest-first) and unbundle them into
/// `git_dir`, then create `refs/bundles/*` refs from the applied bundles so the
/// subsequent clone negotiation can use them as `have`s. Mirrors upstream
/// `fetch_bundle_uri` for the auto-discovered list:
///
/// * bundles are DOWNLOADED newest-first (the order t5601 asserts via trace2),
///   but UNBUNDLED in prerequisite order — a bundle whose prerequisite objects
///   are not yet present is deferred and retried once its prerequisites arrive;
/// * with `bundle.mode = any` a single successfully-applied bundle is enough, so
///   unbundling stops after the first success;
/// * failures are best-effort: a bundle that cannot be downloaded or applied is
///   warned about (upstream's "failed to download bundle from URI" text) and the
///   remaining bundles / the normal negotiation still proceed.
pub fn prefetch_advertised_bundle_uris(
    git_dir: &Path,
    format: ObjectFormat,
    list: &BundleUriList,
) -> Result<()> {
    let client = UreqHttpClient::new();
    prefetch_advertised_bundle_uris_with_client(&client, git_dir, format, list)
}

/// Native, injectable variant of [`prefetch_advertised_bundle_uris`].
///
/// Supplying the client keeps bundle prefetch available to embedders without
/// consulting `PATH`, `GIT_EXEC_PATH`, an upstream Git build, or an installed
/// Git executable. Filesystem bundle URIs do not use `client`.
pub fn prefetch_advertised_bundle_uris_with_client(
    client: &dyn HttpClient,
    git_dir: &Path,
    format: ObjectFormat,
    list: &BundleUriList,
) -> Result<()> {
    if list.bundles.is_empty() {
        return Ok(());
    }
    // Download in advertised (newest-first) order; keep the downloaded temp file
    // alongside the id so it can be unbundled later in prerequisite order.
    let mut downloaded: Vec<(String, PathBuf)> = Vec::new();
    for entry in ordered_bundle_entries(list) {
        let Some(uri) = entry.uri.as_deref() else {
            continue;
        };
        if uri.is_empty() {
            continue;
        }
        trace2_bundle_uri_download(uri);
        match download_bundle_uri_to_temp(client, uri) {
            Ok(temp) => downloaded.push((entry.id.clone(), temp)),
            Err(_) => {
                eprintln!("warning: failed to download bundle from URI '{uri}'");
            }
        }
    }

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let ref_store = FileRefStore::new(git_dir, format);
    let any_mode = list.mode == BundleMode::Any;

    // Unbundle in prerequisite order: repeatedly apply any downloaded bundle
    // whose prerequisites are already satisfied, until no further progress is
    // made. This handles thin range bundles (e.g. `HEAD~1..HEAD`) whose base
    // objects come from an earlier (full) bundle regardless of download order.
    let mut pending = downloaded;
    let mut applied_any = false;
    loop {
        let mut progressed = false;
        let mut still_pending: Vec<(String, PathBuf)> = Vec::new();
        for (id, temp) in pending.drain(..) {
            match apply_bundle_file(&temp, format, &db, &ref_store) {
                Ok(true) => {
                    progressed = true;
                    applied_any = true;
                    let _ = fs::remove_file(&temp);
                    if any_mode {
                        // A single bundle satisfies `bundle.mode = any`; drop the
                        // rest without applying them.
                        for (_, leftover) in still_pending {
                            let _ = fs::remove_file(&leftover);
                        }
                        return Ok(());
                    }
                }
                Ok(false) => still_pending.push((id, temp)),
                Err(_) => {
                    // A genuinely broken bundle (not merely a missing
                    // prerequisite) is dropped with a warning.
                    let _ = fs::remove_file(&temp);
                    progressed = true;
                }
            }
        }
        pending = still_pending;
        if pending.is_empty() || !progressed {
            break;
        }
    }
    // Any bundles still pending had unsatisfiable prerequisites; clean them up.
    for (_, temp) in pending {
        let _ = fs::remove_file(&temp);
    }
    let _ = applied_any;
    Ok(())
}

/// Record the compatibility child boundary Git exposes while downloading an
/// advertised HTTP bundle. Sley performs the transfer in-process, but trace2 is
/// a public observability surface: callers still expect the logical
/// `git-remote-https <uri>` child in download order.
fn trace2_bundle_uri_download(uri: &str) {
    if let Some(argv) = bundle_uri_trace_argv(uri) {
        sley_core::trace2::child_start("remote-https", &argv);
    }
}

fn bundle_uri_trace_argv(uri: &str) -> Option<[String; 2]> {
    (uri.starts_with("http://") || uri.starts_with("https://"))
        .then(|| ["git-remote-https".to_string(), uri.to_string()])
}

/// Try to unbundle `path` into `db` and create its `refs/bundles/*` refs.
/// Returns `Ok(true)` when the bundle was applied, `Ok(false)` when it could not
/// be applied yet because a prerequisite object is still missing (so the caller
/// should retry after applying other bundles), or `Err` for a corrupt bundle.
fn apply_bundle_file(
    path: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ref_store: &FileRefStore,
) -> Result<bool> {
    let bytes = fs::read(path)?;
    let bundle = Bundle::parse(&bytes, format)?;
    if verify_bundle_prerequisites(&bundle, db).is_err() {
        // A prerequisite object is not present yet: defer.
        return Ok(false);
    }
    let result = install_bundle_pack(&bundle, db, db)?;
    create_bundle_refs(ref_store, &result.references);
    Ok(true)
}

/// Create `refs/bundles/<branch>` refs from the applied bundle's tips (upstream
/// `unbundle_from_file` converting `refs/*` into `refs/bundles/*`). These refs
/// keep the fetched objects reachable and are advertised as `have`s during the
/// subsequent clone negotiation.
fn create_bundle_refs(ref_store: &FileRefStore, references: &[sley_formats::BundleReference]) {
    for reference in references {
        let Some(branch) = reference.name.strip_prefix("refs/") else {
            continue;
        };
        let mut tx = ref_store.transaction();
        tx.update(RefUpdate {
            name: format!("refs/bundles/{branch}"),
            expected: None,
            new: RefTarget::Direct(reference.oid),
            reflog: None,
        });
        let _ = tx.commit();
    }
}

/// Download `uri` to a fresh temp file. HTTP(S) URIs use Sley's native client;
/// `file://` and plain-path URIs are COPIED into a temp file
/// (never handed back as the original path — the caller deletes the returned
/// path after installing, and must not delete the user's bundle, cf. upstream
/// `copy_uri_to_file`).
fn download_bundle_uri_to_temp(client: &dyn HttpClient, uri: &str) -> Result<PathBuf> {
    let temp = unique_bundle_temp_path();
    if uri.starts_with("http://") || uri.starts_with("https://") {
        download_http_bundle_uri(client, uri, &temp)?;
        return Ok(temp);
    }
    let source = uri.strip_prefix("file://").unwrap_or(uri);
    fs::copy(source, &temp).map_err(|err| GitError::Io(err.to_string()))?;
    Ok(temp)
}

fn unique_bundle_temp_path() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("sley-bundle-uri-{nanos}-{seq}"))
}

/// Download a static HTTP(S) bundle through Sley's native HTTP client.
///
/// The response is streamed to disk so large bundles are never buffered in
/// memory. A partial destination is removed on every error.
fn download_http_bundle_uri(client: &dyn HttpClient, uri: &str, dest: &Path) -> Result<()> {
    if uri.contains(' ') || uri.contains('\n') {
        return Err(GitError::InvalidFormat(format!(
            "bundle-uri: URI is malformed: '{uri}'"
        )));
    }
    let dest_display = dest.to_string_lossy();
    if dest_display.contains('\n') {
        return Err(GitError::InvalidFormat(format!(
            "bundle-uri: filename is malformed: '{dest_display}'"
        )));
    }
    let result = (|| -> Result<()> {
        let mut response = client.get(uri, &[])?;
        http_check_status(&response, uri)?;
        let mut output = fs::File::create(dest).map_err(|err| GitError::Io(err.to_string()))?;
        std::io::copy(&mut response.body, &mut output)
            .map_err(|err| GitError::Io(err.to_string()))?;
        output
            .flush()
            .map_err(|err| GitError::Io(err.to_string()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

/// Parse a bundle URI through the transport's standard remote URL parser.
///
/// Kept as the bundle-specific facade for embedders even though the in-crate
/// fetch path currently accepts URI strings directly.
#[allow(dead_code)]
pub fn remote_url_from_bundle_uri(uri: &str) -> Result<RemoteUrl> {
    parse_remote_url(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticHttpClient {
        status: u16,
        body: &'static [u8],
    }

    impl HttpClient for StaticHttpClient {
        fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> Result<sley_transport::HttpResponse> {
            Ok(sley_transport::HttpResponse {
                status: self.status,
                content_type: Some("application/octet-stream".into()),
                content_length: None,
                content_range: None,
                body: Box::new(std::io::Cursor::new(self.body)),
            })
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<sley_transport::HttpResponse> {
            Err(GitError::Unsupported(
                "unexpected POST in bundle download".into(),
            ))
        }
    }

    fn sample_list() -> BundleUriList {
        BundleUriList {
            version: 1,
            mode: BundleMode::All,
            creation_token_heuristic: true,
            base_uri: "http://127.0.0.1:18080/smart/repo4.git".into(),
            ..BundleUriList::default()
        }
    }

    #[test]
    fn bundle_uri_fetch_order_sorts_by_creation_token_descending() {
        let mut list = sample_list();
        for (id, token, uri) in [
            ("everything", 1, "http://127.0.0.1:18080/everything.bundle"),
            ("new", 2, "http://127.0.0.1:18080/new.bundle"),
            ("newest", 3, "http://127.0.0.1:18080/newest.bundle"),
        ] {
            list.bundles.insert(
                id.to_string(),
                BundleUriEntry {
                    id: id.to_string(),
                    uri: Some(uri.to_string()),
                    creation_token: token,
                },
            );
        }
        assert_eq!(
            bundle_uri_fetch_order(&list),
            vec![
                "http://127.0.0.1:18080/newest.bundle".to_string(),
                "http://127.0.0.1:18080/new.bundle".to_string(),
                "http://127.0.0.1:18080/everything.bundle".to_string(),
            ]
        );
    }

    #[test]
    fn bundle_uri_trace_models_only_http_remote_helper_boundaries() {
        assert_eq!(
            bundle_uri_trace_argv("http://example.test/repo.bundle"),
            Some([
                "git-remote-https".to_string(),
                "http://example.test/repo.bundle".to_string(),
            ])
        );
        assert_eq!(
            bundle_uri_trace_argv("https://example.test/repo.bundle"),
            Some([
                "git-remote-https".to_string(),
                "https://example.test/repo.bundle".to_string(),
            ])
        );
        assert_eq!(bundle_uri_trace_argv("file:///tmp/repo.bundle"), None);
    }

    #[test]
    fn native_http_bundle_download_streams_response_to_file() {
        let destination = unique_bundle_temp_path();
        let client = StaticHttpClient {
            status: 200,
            body: b"native bundle bytes",
        };
        download_http_bundle_uri(&client, "https://example.test/repo.bundle", &destination)
            .expect("native bundle download");
        assert_eq!(
            fs::read(&destination).expect("downloaded bundle"),
            b"native bundle bytes"
        );
        let _ = fs::remove_file(destination);
    }

    #[test]
    fn native_http_bundle_download_removes_error_destination() {
        let destination = unique_bundle_temp_path();
        fs::write(&destination, b"stale").expect("seed stale destination");
        let client = StaticHttpClient {
            status: 404,
            body: b"not found",
        };
        assert!(
            download_http_bundle_uri(&client, "https://example.test/missing.bundle", &destination)
                .is_err()
        );
        assert!(!destination.exists());
    }

    #[test]
    fn parse_bundle_uri_line_accepts_creation_token_heuristic_lines() {
        let mut list = BundleUriList::default();
        parse_bundle_uri_line(&mut list, "bundle.heuristic=creationToken")
            .expect("test operation should succeed");
        assert!(list.creation_token_heuristic);
        parse_bundle_uri_line(&mut list, "bundle.newest.creationtoken=3")
            .expect("test operation should succeed");
        parse_bundle_uri_line(
            &mut list,
            "bundle.newest.uri=http://127.0.0.1/newest.bundle",
        )
        .expect("test operation should succeed");
        let entry = list.bundles.get("newest").expect("bundle entry");
        assert_eq!(entry.creation_token, 3);
        assert_eq!(entry.uri.as_deref(), Some("http://127.0.0.1/newest.bundle"));
    }

    #[test]
    fn parse_config_key_splits_subsection_at_last_dot() {
        assert_eq!(
            parse_bundle_config_key("bundle.version"),
            Some((None, "version"))
        );
        assert_eq!(parse_bundle_config_key("bundle.mode"), Some((None, "mode")));
        assert_eq!(
            parse_bundle_config_key("bundle.everything.uri"),
            Some((Some("everything"), "uri"))
        );
        // A subsection that itself contains dots splits at the LAST dot only.
        assert_eq!(
            parse_bundle_config_key("bundle.my.nested.id.creationtoken"),
            Some((Some("my.nested.id"), "creationtoken"))
        );
        // Not under the `bundle` section (case-sensitive) → no match.
        assert_eq!(parse_bundle_config_key("Bundle.version"), None);
        assert_eq!(parse_bundle_config_key("transfer.bundleuri"), None);
    }

    #[test]
    fn parse_bundle_uri_line_is_byte_exact_and_untrimmed() {
        let mut list = BundleUriList::default();
        // Mixed-case keys/values are NOT normalized: `bundle.mode = All` is an
        // unrecognized value (upstream `strcmp`), so it errors.
        assert!(parse_bundle_uri_line(&mut list, "bundle.mode=All").is_err());
        // A trailing space is part of the value, so `all ` is not `all`.
        assert!(parse_bundle_uri_line(&mut list, "bundle.mode=all ").is_err());
        // Exact match works.
        parse_bundle_uri_line(&mut list, "bundle.mode=all").expect("mode=all parses");
        assert_eq!(list.mode, BundleMode::All);
        parse_bundle_uri_line(&mut list, "bundle.mode=any").expect("mode=any parses");
        assert_eq!(list.mode, BundleMode::Any);
    }

    #[test]
    fn parse_bundle_uri_line_rejects_unsupported_version_and_bad_key() {
        let mut list = BundleUriList::default();
        assert!(parse_bundle_uri_line(&mut list, "bundle.version=2").is_err());
        assert!(parse_bundle_uri_line(&mut list, "bundle.version=notanumber").is_err());
        // Key with an empty value or empty key is rejected.
        assert!(parse_bundle_uri_line(&mut list, "bundle.version=").is_err());
        assert!(parse_bundle_uri_line(&mut list, "=value").is_err());
    }

    #[test]
    fn relative_bundle_uri_passes_absolute_values_through_unchanged() {
        let base = "http://example.com/smart/repo.git";
        assert_eq!(
            relative_bundle_uri(base, "http://cdn.example.com/x.bundle"),
            "http://cdn.example.com/x.bundle"
        );
        // Absolute filesystem paths pass through unchanged (upstream relative_url
        // returns absolute paths as-is).
        assert_eq!(relative_bundle_uri(base, "/srv/x.bundle"), "/srv/x.bundle");
        // Genuinely relative values are joined onto the base.
        assert_eq!(
            relative_bundle_uri(base, "x.bundle"),
            "http://example.com/smart/repo.git/x.bundle"
        );
    }

    #[test]
    fn parse_bundle_uri_line_rejects_duplicate_uri() {
        let mut list = BundleUriList::default();
        parse_bundle_uri_line(&mut list, "bundle.a.uri=http://x/a.bundle").expect("first uri");
        assert!(parse_bundle_uri_line(&mut list, "bundle.a.uri=http://x/b.bundle").is_err());
    }
}
