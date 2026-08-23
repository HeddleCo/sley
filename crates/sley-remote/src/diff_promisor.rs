//! Promisor lazy-fetch support for diff rendering: batch blob hydration and
//! single-object reads, mirroring git's `diff_queued_diff_prefetch` +
//! `promisor_remote_get_direct`.
//!
//! Hosts inject their effective-config reader (includes plus command-line
//! overrides) through [`LoadRepoConfig`]; without it promisor remotes cannot
//! be discovered and prefetching degrades to a no-op.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectId, Result};
use sley_diff_merge::NameStatusEntry;
use sley_object::EncodedObject;
use sley_odb::{FileObjectDatabase, ObjectReader};

/// Reads the effective repository config for `git_dir`; `None` when
/// unreadable.
pub type LoadRepoConfig<'a> = &'a (dyn Fn(&Path) -> Option<GitConfig> + 'a);

/// Promisor remotes to consult for a lazy fetch, in Git's
/// `promisor_remote_get_direct` order.
pub fn promisor_remote_names(config: &GitConfig) -> Vec<String> {
    crate::configured_promisor_remote_names(config)
}

/// Read an object from `db`, lazily hydrating it from a configured promisor
/// remote when missing. Assumes lazy fetching is enabled; callers gate by not
/// invoking this when it is not.
///
/// # Errors
/// Propagates read/fetch errors; a missing object that no promisor supplied
/// surfaces the original `NotFound`.
pub fn read_object_maybe_prefetch_promisor(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    load_repo_config: LoadRepoConfig<'_>,
) -> Result<Arc<EncodedObject>> {
    let object = match db.read_object(oid) {
        Ok(object) => object,
        Err(err @ GitError::NotFound(_)) => {
            if !prefetch_local_promisor_object(db, oid, load_repo_config)? {
                return Err(err);
            }
            db.read_object(oid)?
        }
        Err(err) => return Err(err),
    };
    Ok(object)
}

/// Batch-prefetch every missing blob referenced by the queued diff entries.
/// Mirrors git's `diff_queued_diff_prefetch` + `promisor_remote_get_direct`.
pub fn prefetch_diff_entry_blobs(
    db: &FileObjectDatabase,
    entries: &[NameStatusEntry],
    new_side_is_worktree: bool,
    load_repo_config: LoadRepoConfig<'_>,
) -> Result<()> {
    let oids = if new_side_is_worktree {
        let mut seen = HashSet::new();
        entries
            .iter()
            .filter(|entry| entry.old_mode != Some(0o160000))
            .filter_map(|entry| entry.old_oid)
            .filter(|oid| seen.insert(*oid))
            .collect()
    } else {
        sley_diff_merge::porcelain::collect_diff_entry_blob_oids(entries)
    };
    prefetch_promisor_objects(db, &oids, load_repo_config)
}

/// Materialize the missing subset of `oids` in one request per configured
/// local/file promisor. Packet-trace identity is `fetch` for the duration of
/// each negotiation so `GIT_TRACE_PACKET` matches git's child-fetch process
/// (t4067, t1022).
pub fn prefetch_promisor_objects(
    db: &FileObjectDatabase,
    oids: &[ObjectId],
    load_repo_config: LoadRepoConfig<'_>,
) -> Result<()> {
    if oids.is_empty() {
        return Ok(());
    }

    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for oid in oids {
        if seen.insert(*oid) && !db.contains(oid)? {
            missing.push(*oid);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let Some(git_dir) = database_git_dir(db) else {
        return Ok(());
    };
    let Some(config) = load_repo_config(&git_dir) else {
        return Ok(());
    };
    let resolution_cwd =
        sley_worktree::worktree_root_for_git_dir(&git_dir)?.unwrap_or_else(|| git_dir.clone());

    // In-process upload-pack reuses this process's packet-trace identity; git's
    // promisor path forks `git fetch`, so traces show `fetch> done`. Match that.
    let _trace_identity = sley_protocol::scoped_packet_trace_identity("fetch");

    for remote_name in promisor_remote_names(&config) {
        if missing.is_empty() {
            break;
        }
        // Custom upload-pack is an arbitrary shell protocol; leave those to the
        // single-object fallback rather than inventing a stdin protocol here.
        if config
            .get("remote", Some(&remote_name), "uploadpack")
            .is_some()
        {
            continue;
        }
        let Some(url) = config.get("remote", Some(&remote_name), "url") else {
            continue;
        };
        let resolution = crate::RemoteResolutionContext {
            cwd: &resolution_cwd,
            local_git_dir: Some(&git_dir),
            config: Some(&config),
        };
        let filter = config
            .get("remote", Some(&remote_name), "partialclonefilter")
            .and_then(crate::pack_filter_from_spec)
            .or(Some(sley_odb::PackObjectFilter::BlobNone));
        let quiet = config.get_bool("promisor", None, "quiet").unwrap_or(false);
        trace2_promisor_fetch_child_start(&remote_name, quiet);
        let hydrated_ok = if let Ok(remote_git_dir) =
            crate::resolve_local_remote_git_dir(resolution, url)
        {
            crate::install_fetch_pack_via_local_upload_pack(
                &git_dir,
                &remote_git_dir,
                db.object_format(),
                missing.clone(),
                None,
                true,
                false,
                filter,
                None,
                false,
                None,
            )
            .is_ok()
        } else {
            maybe_hydrate_promisor_via_http(&git_dir, db, url, &missing, filter.clone())
                .unwrap_or_default()
        };
        if !hydrated_ok {
            continue;
        }

        db.refresh_read_cache();
        let before = missing.len();
        let mut still_missing = Vec::with_capacity(before);
        for oid in missing {
            if !db.contains(&oid)? {
                still_missing.push(oid);
            }
        }
        missing = still_missing;
        let hydrated = before - missing.len();
        if hydrated > 0 {
            sley_core::trace2::data("promisor", "fetch_count", hydrated as u64);
            sley_core::trace2::data("pack-objects", "written", hydrated as u64);
        }
    }
    Ok(())
}

fn prefetch_local_promisor_object(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    load_repo_config: LoadRepoConfig<'_>,
) -> Result<bool> {
    // Prefer the batched path when a single oid is requested so packet identity
    // and fetch_count accounting stay consistent with multi-oid callers.
    let before = db.contains(oid).unwrap_or(false);
    if before {
        return Ok(false);
    }
    prefetch_promisor_objects(db, &[*oid], load_repo_config)?;
    if db.contains(oid).unwrap_or(false) {
        return Ok(true);
    }
    // Fallback: custom remote.<name>.uploadpack (not handled by the batch path).
    let Some(git_dir) = database_git_dir(db) else {
        return Ok(false);
    };
    let Some(config) = load_repo_config(&git_dir) else {
        return Ok(false);
    };
    for remote_name in promisor_remote_names(&config) {
        let Some(url) = config.get("remote", Some(&remote_name), "url") else {
            continue;
        };
        let quiet = config.get_bool("promisor", None, "quiet").unwrap_or(false);
        if let Some(command) = config.get("remote", Some(&remote_name), "uploadpack") {
            trace2_promisor_fetch_child_start(&remote_name, quiet);
            let _ = prefetch_via_configured_upload_pack(command, url)?;
            db.refresh_read_cache();
            if db.contains(oid).unwrap_or(false) {
                return Ok(true);
            }
            return Ok(false);
        }
    }
    Ok(false)
}

/// Smart-HTTP promisor hydrate (t0410 #39): exact-want, no haves. Returns
/// `Some(any_hydrated)` when `url` is an HTTP(S) remote this build can service,
/// or `None` when the URL is not HTTP so the caller can fall through. Only the
/// `http` feature carries the smart-HTTP transport, so without it every URL
/// falls through here.
#[cfg(feature = "http")]
fn maybe_hydrate_promisor_via_http(
    git_dir: &Path,
    db: &FileObjectDatabase,
    url: &str,
    missing: &[ObjectId],
    filter: Option<sley_odb::PackObjectFilter>,
) -> Option<bool> {
    if !crate::remote_url_is_http(url).unwrap_or(false) {
        return None;
    }
    let mut any = false;
    for oid in missing {
        if hydrate_promisor_oid_via_http(git_dir, db.object_format(), url, *oid, filter.clone())
            .is_ok()
        {
            any = true;
        }
    }
    Some(any)
}

/// Without the `http` feature there is no smart-HTTP transport, so no URL can
/// be hydrated over HTTP; always fall through to the caller's next path.
#[cfg(not(feature = "http"))]
fn maybe_hydrate_promisor_via_http(
    _git_dir: &Path,
    _db: &FileObjectDatabase,
    _url: &str,
    _missing: &[ObjectId],
    _filter: Option<sley_odb::PackObjectFilter>,
) -> Option<bool> {
    None
}

/// Lazy-fetch one missing object from a smart-HTTP promisor remote.
///
/// Mirrors git's `promisor_remote_get_direct` over HTTP: exact-want, no haves,
/// installed as a promisor pack so subsequent fsck/rev-list still treat the
/// transfer as partial.
#[cfg(feature = "http")]
#[allow(clippy::too_many_arguments)]
fn hydrate_promisor_oid_via_http(
    git_dir: &Path,
    format: sley_core::ObjectFormat,
    url: &str,
    oid: ObjectId,
    _filter: Option<sley_odb::PackObjectFilter>,
) -> Result<()> {
    let remote = sley_transport::parse_remote_url(url)?;
    if !matches!(
        remote.transport,
        sley_transport::RemoteTransport::Http | sley_transport::RemoteTransport::Https
    ) {
        return Err(GitError::Unsupported(
            "promisor HTTP hydrate requires HTTP(S)".into(),
        ));
    }
    let client = crate::new_http_client();
    let mut credentials = crate::CredentialHelperProvider::new(None);
    let discovered = crate::http_service_advertisements(
        &client,
        &remote,
        format,
        sley_protocol::GitService::UploadPack,
        &mut credentials,
        None,
    )?;
    let pack_request = crate::HttpFetchPackRequest {
        client: &client,
        git_dir,
        format,
        remote: &remote,
        wants: vec![oid],
        haves: None,
        shallow: Vec::new(),
        deepen: None,
        promisor: true,
        max_input_size: None,
        // Omit a partial-clone filter on the wire: many HTTP remotes (including
        // t0410's plain smart-HTTP fixture) have not set `uploadpack.allowfilter`,
        // and exact-object hydration only needs the named wants (t0410 #39).
        // Local promisor fetches keep their blob:none filter separately.
        filter: None,
        packfile_uri_protocols: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative: false,
        git_protocol: Some("version=2"),
        post_buffer: 1 << 20,
        omit_haves: true,
    };
    let mut progress = crate::SilentProgress;
    if let Some(handshake) = discovered.handshake.as_ref() {
        crate::install_fetch_pack_via_http_protocol_v2_fetch(
            pack_request,
            handshake,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    } else {
        crate::install_fetch_pack_via_http_upload_pack(
            pack_request,
            &mut credentials,
            &mut progress,
            sley_core::CancelFlag::never(),
        )?;
    }
    Ok(())
}

pub fn prefetch_via_configured_upload_pack(command: &str, repository: &str) -> Result<bool> {
    let command = format!("{command} {}", sley_config::sq_quote(repository));
    let output = ProcessCommand::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()?;
    io::stderr().write_all(&output.stderr)?;
    Ok(output.status.success())
}

fn database_git_dir(db: &FileObjectDatabase) -> Option<PathBuf> {
    let objects = db.objects_dir();
    (objects.file_name()? == "objects").then(|| objects.parent().map(Path::to_path_buf))?
}

fn trace2_promisor_fetch_child_start(remote_name: &str, quiet: bool) {
    let Some(path) = env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    let mut argv = vec![
        "git",
        "-c",
        "fetch.negotiationAlgorithm=noop",
        "fetch",
        remote_name,
        "--no-tags",
        "--no-write-fetch-head",
        "--recurse-submodules=no",
        "--stdin",
    ];
    if quiet {
        argv.push("--quiet");
    }
    let argv = argv
        .iter()
        .map(|arg| format!("\"{}\"", trace2_json_escape(arg)))
        .collect::<Vec<_>>()
        .join(",");
    let line =
        format!("{{\"event\":\"child_start\",\"sid\":\"sley\",\"child_id\":0,\"argv\":[{argv}]}}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn trace2_json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}
