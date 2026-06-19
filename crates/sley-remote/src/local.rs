//! In-process `file://` upload-pack / receive-pack server.
//!
//! These are the transport-independent cores behind `git upload-pack` /
//! `git receive-pack` and the local fetch/push paths: given a `git_dir`, an
//! [`ObjectFormat`], and a decoded request, they read/write refs and objects
//! through [`sley_refs`]/[`sley_odb`] and run the [`sley_protocol`] server logic.
//! They take everything as explicit parameters and never touch process-global
//! state, argument parsing, or stdout/stderr, so the CLI's `cmd_upload_pack` /
//! `cmd_receive_pack` stdio wrappers and the `fetch`/`push` orchestration can
//! call them, and an embedder can drive them directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sley_core::{
    Capability, GitError, ObjectFormat, ObjectId, Result, UPSTREAM_GIT_COMPAT_VERSION,
};
use sley_object::{Commit, ObjectType, Tag};
use sley_odb::{
    FileObjectDatabase, ObjectReader, RawPackInstallOptions, build_and_install_reachable_pack,
    build_and_install_reachable_pack_filtered, build_reachable_pack, collect_reachable_object_ids,
};
use sley_protocol::{
    PKT_LINE_MAX_PAYLOAD_LEN, ProtocolV2FetchAcknowledgment,
    ProtocolV2FetchRequest, ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo,
    ProtocolV2LsRefsRecord, ProtocolV2LsRefsRef, ProtocolV2LsRefsRequest, ProtocolVersion,
    ReceivePackFeatures, ReceivePackPushRequest, ReceivePackReportStatus, ReceivePackRequest,
    RefAdvertisement, SideBandChannel, SideBandPacket, TransportHandshake, UploadPackFeatures,
    UploadPackNegotiationRequest, UploadPackPackfileResponse, UploadPackRawPackfileResponse,
    UploadPackRequest, apply_receive_pack_push_request, build_upload_pack_raw_packfile_response,
    encode_receive_pack_features, encode_upload_pack_features,
    read_protocol_v2_command_request, read_upload_pack_negotiation_request, read_upload_pack_request,
    write_protocol_v2_advertisement, write_protocol_v2_fetch_response,
    write_protocol_v2_ls_refs_response, write_upload_pack_negotiation_request,
    write_upload_pack_request,
};
use sley_refs::{DeleteRef, FileRefStore, Ref, RefPrecondition, RefTarget, ReflogEntry};

/// The all-zero object id for `format`, used for the synthetic
/// `capabilities^{}` advertisement when a repository has no refs.
fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    Ok(ObjectId::null(format))
}

/// Resolve a (possibly symbolic) ref target to its object id, following up to
/// five levels of symbolic indirection, returning the first symbolic name seen.
fn resolve_for_each_ref_target(
    store: &FileRefStore,
    reference: &Ref,
) -> Result<Option<(ObjectId, Option<String>)>> {
    let mut target = reference.target.clone();
    let mut symref = None;
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some((oid, symref))),
            RefTarget::Symbolic(name) => {
                symref.get_or_insert_with(|| name.clone());
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}

/// The upload-pack capabilities advertised for the repository at `git_dir`:
/// the object format, side-band-64k, and a `HEAD` symref hint if present.
pub fn upload_pack_features(git_dir: &Path, format: ObjectFormat) -> Result<UploadPackFeatures> {
    let store = FileRefStore::new(git_dir, format);
    let mut symrefs = Vec::new();
    if let Some(RefTarget::Symbolic(target)) = store.read_ref("HEAD")? {
        symrefs.push(format!("HEAD:{target}"));
    }
    Ok(UploadPackFeatures {
        object_format: Some(format),
        side_band_64k: true,
        symrefs,
        ..UploadPackFeatures::default()
    })
}

/// Whether the client negotiated a side-band channel for the packfile response.
pub fn upload_pack_request_uses_sideband(request: &UploadPackRequest) -> bool {
    request
        .capabilities
        .iter()
        .any(|capability| matches!(capability.name.as_str(), "side-band" | "side-band-64k"))
}

/// Re-frame a raw packfile response as side-band data packets, chunked to the
/// pkt-line payload limit (less the one-byte channel prefix).
pub fn upload_pack_sideband_response(
    response: UploadPackRawPackfileResponse,
) -> UploadPackPackfileResponse {
    let mut sideband = Vec::new();
    let chunk_len = PKT_LINE_MAX_PAYLOAD_LEN - 1;
    for chunk in response.packfile.chunks(chunk_len) {
        sideband.push(SideBandPacket {
            channel: SideBandChannel::Data,
            data: chunk.to_vec(),
        });
    }
    UploadPackPackfileResponse {
        acknowledgments: response.acknowledgments,
        sideband,
    }
}

/// Encode `features` into the leading ref advertisement's capability list,
/// inserting a synthetic `capabilities^{}` entry when there are no refs.
pub fn attach_upload_pack_capabilities(
    advertisements: &mut Vec<RefAdvertisement>,
    format: ObjectFormat,
    features: &UploadPackFeatures,
) -> Result<()> {
    let capabilities = encode_upload_pack_features(features)?;
    if let Some(first) = advertisements.first_mut() {
        first.capabilities = capabilities;
    } else {
        advertisements.push(RefAdvertisement {
            oid: zero_oid(format)?,
            name: "capabilities^{}".into(),
            capabilities,
        });
    }
    Ok(())
}

/// Serve an upload-pack request from the repository at `git_dir`: build the
/// packfile that carries every reachable object the client `wants` but does not
/// already `haves`, framed as a raw (non-side-band) response.
pub fn upload_pack_from_local_repository(
    git_dir: &Path,
    format: ObjectFormat,
    features: &UploadPackFeatures,
    request: UploadPackRequest,
    haves: HashSet<ObjectId>,
) -> Result<UploadPackRawPackfileResponse> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    build_upload_pack_raw_packfile_response(
        features,
        request,
        haves,
        |oid| db.contains(oid),
        |wants, known_haves| {
            let excluded = collect_reachable_object_ids(&db, format, known_haves)?;
            build_reachable_pack(&db, format, wants, &excluded)
                .map(|pack| pack.map(|pack| pack.pack))
        },
    )
}

/// The receive-pack capabilities advertised for a local repository: report
/// status, ref deletion, ofs-delta, push-options, quiet, and the object format.
pub fn receive_pack_features(format: ObjectFormat) -> ReceivePackFeatures {
    ReceivePackFeatures {
        report_status: true,
        delete_refs: true,
        ofs_delta: true,
        push_options: true,
        quiet: true,
        object_format: Some(format),
        ..ReceivePackFeatures::default()
    }
}

/// Whether the client negotiated `push-options` (so the caller must read the
/// push-option section that follows the command list).
pub fn receive_pack_request_uses_push_options(request: &ReceivePackRequest) -> bool {
    request
        .capabilities
        .iter()
        .any(|capability| capability.name == "push-options")
}

/// Encode `features` into the leading ref advertisement's capability list,
/// inserting a synthetic `capabilities^{}` entry when there are no refs.
pub fn attach_receive_pack_capabilities(
    advertisements: &mut Vec<RefAdvertisement>,
    format: ObjectFormat,
    features: &ReceivePackFeatures,
) -> Result<()> {
    let capabilities = encode_receive_pack_features(features)?;
    if let Some(first) = advertisements.first_mut() {
        first.capabilities = capabilities;
    } else {
        advertisements.push(RefAdvertisement {
            oid: zero_oid(format)?,
            name: "capabilities^{}".into(),
            capabilities,
        });
    }
    Ok(())
}

/// Apply a receive-pack push to the repository at `remote_git_dir`: install the
/// incoming packfile and execute the ref creations/updates/deletions, returning
/// the report-status describing what happened.
pub fn receive_pack_into_local_repository(
    remote_git_dir: &Path,
    format: ObjectFormat,
    request: &ReceivePackPushRequest,
) -> Result<ReceivePackReportStatus> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    apply_receive_pack_push_request(
        &receive_pack_features(format),
        request,
        |name| match remote_store.read_ref(name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            Some(RefTarget::Symbolic(_)) | None => Ok(None),
        },
        |packfile| remote_db.install_raw_pack(packfile).map(|_| ()),
        |oid| remote_db.contains(oid),
        |commands| {
            let mut tx = remote_store.transaction();
            let log_updates = receive_pack_log_all_ref_updates(remote_git_dir);
            for command in commands {
                let precondition = if command.old_id.is_null() {
                    RefPrecondition::MustNotExist
                } else {
                    RefPrecondition::MustExistAndMatch(RefTarget::Direct(command.old_id))
                };
                let reflog = if log_updates && receive_pack_should_write_reflog(&command.name) {
                    Some(receive_pack_reflog_entry(format, command.old_id, command.new_id))
                } else {
                    None
                };
                tx.update_to(
                    command.name.clone(),
                    RefTarget::Direct(command.new_id),
                    precondition,
                    reflog,
                );
            }
            tx.commit()
        },
        |command| {
            remote_store
                .delete_ref_checked(DeleteRef {
                    name: command.name.clone(),
                    expected_old: (!command.old_id.is_null()).then_some(command.old_id),
                    reflog: None,
                })
                .map(|_| ())
                .map_err(|err| GitError::Transaction(err.to_string()))
        },
    )
}

fn receive_pack_log_all_ref_updates(git_dir: &Path) -> bool {
    let Ok(config) = fs::read_to_string(git_dir.join("config")) else {
        return false;
    };
    let mut in_core = false;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_core = line.eq_ignore_ascii_case("[core]");
            continue;
        }
        if !in_core || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("logallrefupdates") {
            return matches!(
                value.trim().trim_matches('"').to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1" | "always"
            );
        }
    }
    false
}

fn receive_pack_should_write_reflog(refname: &str) -> bool {
    refname == "HEAD"
        || refname.starts_with("refs/heads/")
        || refname.starts_with("refs/remotes/")
        || refname.starts_with("refs/notes/")
}

fn receive_pack_reflog_entry(
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
) -> ReflogEntry {
    let old_oid = if old_oid.is_null() {
        ObjectId::null(format)
    } else {
        old_oid
    };
    ReflogEntry {
        old_oid,
        new_oid,
        committer: receive_pack_reflog_committer(),
        message: b"push".to_vec(),
    }
}

fn receive_pack_reflog_committer() -> Vec<u8> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("Git Rs <sley@example.invalid> {seconds} +0000").into_bytes()
}

/// Apply a local receive-pack request whose pack can be built from `source_db`
/// after receive-pack preflight checks pass.
///
/// This keeps local push on the same validation path as raw receive-pack while
/// avoiding a raw-pack round trip: the install closure builds the reachable
/// pack and installs the generated pack/index directly.
pub fn receive_pack_reachable_pack_into_local_repository(
    remote_git_dir: &Path,
    format: ObjectFormat,
    request: &ReceivePackPushRequest,
    source_db: &FileObjectDatabase,
    starts: Vec<ObjectId>,
    excluded: HashSet<ObjectId>,
) -> Result<ReceivePackReportStatus> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    let mut starts = Some(starts);
    apply_receive_pack_push_request(
        &receive_pack_features(format),
        request,
        |name| match remote_store.read_ref(name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            Some(RefTarget::Symbolic(_)) | None => Ok(None),
        },
        |_| {
            let starts = starts.take().ok_or_else(|| {
                GitError::InvalidFormat("receive-pack attempted to install pack twice".into())
            })?;
            build_and_install_reachable_pack(
                source_db,
                &remote_db,
                format,
                starts,
                &excluded,
                RawPackInstallOptions { promisor: false },
            )?;
            Ok(())
        },
        |oid| remote_db.contains(oid),
        |commands| {
            let mut tx = remote_store.transaction();
            let log_updates = receive_pack_log_all_ref_updates(remote_git_dir);
            for command in commands {
                let precondition = if command.old_id.is_null() {
                    RefPrecondition::MustNotExist
                } else {
                    RefPrecondition::MustExistAndMatch(RefTarget::Direct(command.old_id))
                };
                let reflog = if log_updates && receive_pack_should_write_reflog(&command.name) {
                    Some(receive_pack_reflog_entry(format, command.old_id, command.new_id))
                } else {
                    None
                };
                tx.update_to(
                    command.name.clone(),
                    RefTarget::Direct(command.new_id),
                    precondition,
                    reflog,
                );
            }
            tx.commit()
        },
        |command| {
            remote_store
                .delete_ref_checked(DeleteRef {
                    name: command.name.clone(),
                    expected_old: (!command.old_id.is_null()).then_some(command.old_id),
                    reflog: None,
                })
                .map(|_| ())
                .map_err(|err| GitError::Transaction(err.to_string()))
        },
    )
}

/// The ref advertisements a local repository would send to a fetching client:
/// `HEAD` (if resolvable) followed by every ref, each resolved to its object id.
pub fn local_fetch_advertisements(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<RefAdvertisement>> {
    let store = FileRefStore::new(git_dir, format);
    let mut advertisements = Vec::new();
    if let Some(target) = store.read_ref("HEAD")? {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? {
            advertisements.push(RefAdvertisement {
                oid,
                name: reference.name,
                capabilities: Vec::new(),
            });
        }
    }
    for reference in store.list_refs()? {
        let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        advertisements.push(RefAdvertisement {
            oid,
            name: reference.name,
            capabilities: Vec::new(),
        });
    }
    Ok(advertisements)
}

/// The object ids the local repository can offer as `have`s during negotiation:
/// the deduplicated tips of its own advertisements.
pub fn local_have_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut seen = HashSet::new();
    let mut haves = Vec::new();
    for advertisement in local_fetch_advertisements(git_dir, format)? {
        if seen.insert(advertisement.oid) {
            haves.push(advertisement.oid);
        }
    }
    Ok(haves)
}

/// The in-process upload-pack's plan for a `deepen` (shallow) local fetch:
/// which `shallow`/`unshallow` updates to report, which commits the pack walk
/// must stop at, and which extra tips become packable because the client's
/// boundary moved.
///
/// Mirrors upstream `upload-pack.c::deepen` + `shallow.c::get_shallow_commits`.
#[derive(Debug, Clone)]
pub struct LocalDeepenPlan {
    /// The requested deepen depth (`--depth N`; [`INFINITE_DEPTH`] for
    /// `--unshallow` and for the implicit deepen a shallow server runs on a
    /// plain fetch; `0` for the deepen-since/deepen-not rev-list modes).
    pub depth: u32,
    /// The request carried `deepen-since` (trace2 `fetch-info` parity).
    pub deepen_since: bool,
    /// Number of `deepen-not` entries in the request (trace2 parity).
    pub deepen_not: usize,
    /// The client's existing shallow boundary (`$GIT_DIR/shallow`), replayed as
    /// `shallow` lines in the upload-pack request.
    pub client_shallow: Vec<ObjectId>,
    /// The server's `shallow`/`unshallow` updates the client must fold into
    /// `$GIT_DIR/shallow` after the pack lands (see [`crate::apply_shallow_info`]).
    pub shallow_info: Vec<ProtocolV2FetchShallowInfo>,
    /// Out-of-boundary commits (the parents of boundary commits that are not
    /// themselves within the boundary): excluding these from the pack walk
    /// truncates history at the boundary while keeping every tree/blob of the
    /// boundary commits themselves.
    pub excluded: HashSet<ObjectId>,
    /// Parents of client-shallow commits this deepen un-shallowed, added as
    /// extra pack tips so the newly visible history is sent (upload-pack adds
    /// them to `want_obj` in `send_unshallow`).
    pub extra_wants: Vec<ObjectId>,
}

/// Dereference `oid` through any chain of annotated tags to a commit, or `None`
/// when it ultimately points at a tree or blob (`deref_tag` in upstream
/// `shallow.c`'s boundary walk).
fn peel_to_commit<R: ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let mut oid = *oid;
    loop {
        let object = remote_db.read_object(&oid)?;
        match object.object_type {
            ObjectType::Commit => return Ok(Some(oid)),
            ObjectType::Tag => oid = Tag::parse_ref(format, &object.body)?.object,
            _ => return Ok(None),
        }
    }
}

/// Compute the deepen plan for a shallow local fetch, mirroring upstream
/// `shallow.c::get_shallow_commits`: a breadth-first minimum-depth walk from the
/// (tag-dereferenced) `heads` — the primary planned tips, upload-pack's
/// `want_obj`, NOT auto-followed tags — where tips enter at depth 0 and a commit
/// processed at depth `d` is a boundary commit when `d + 1 >= depth` (it is
/// packed, but its parents are not walked).
///
/// `client_shallow` is the client's current boundary: boundary commits the
/// client already has are not re-reported (`send_shallow` skips
/// `CLIENT_SHALLOW`), and client-shallow commits now within the boundary are
/// reported as `unshallow` with their parents returned as extra pack tips
/// (`send_unshallow`).
pub fn compute_local_deepen<R: ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    heads: &[ObjectId],
    client_shallow: Vec<ObjectId>,
    depth: u32,
    deepen_relative: bool,
) -> Result<LocalDeepenPlan> {
    // `--deepen=N`: the boundary moves N commits past the client's current
    // boundary (upstream `get_shallows_depth` + `depth +=`).
    let depth = if deepen_relative && depth < INFINITE_DEPTH {
        depth.saturating_add(client_shallow_min_depth(
            remote_db,
            format,
            heads,
            &client_shallow,
        )?)
    } else {
        depth
    };
    let mut min_depth: HashMap<ObjectId, u32> = HashMap::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for head in heads {
        let Some(commit) = peel_to_commit(remote_db, format, head)? else {
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = min_depth.entry(commit) {
            entry.insert(0);
            queue.push_back(commit);
        }
    }
    // FIFO processing with uniform edge weight makes the first visit the
    // minimum depth, so each commit is processed exactly once and expands its
    // parents only when it is within the boundary — the same fixpoint as
    // upstream's decrease-key re-walks.
    let mut boundary = Vec::new();
    let mut boundary_parents = HashSet::new();
    while let Some(oid) = queue.pop_front() {
        let commit_depth = min_depth[&oid];
        let object = remote_db.read_object(&oid)?;
        let parents = sley_odb::grafted_parents(
            remote_db,
            &oid,
            Commit::parse_ref(format, &object.body)?.parents,
        );
        // A commit is boundary when the requested depth cuts at it, or when
        // the server's own history is cut at it (a shallow server reports its
        // graft points to the client — upstream `get_shallows_or_depth`).
        if (depth != INFINITE_DEPTH && commit_depth + 1 >= depth)
            || remote_db.is_shallow_graft(&oid)
        {
            boundary.push(oid);
            boundary_parents.extend(parents);
            continue;
        }
        for parent in parents {
            if let std::collections::hash_map::Entry::Vacant(entry) = min_depth.entry(parent) {
                entry.insert(commit_depth + 1);
                queue.push_back(parent);
            }
        }
    }
    // A boundary commit's parent can itself be within the boundary via a
    // shorter path (and is then packed); only parents the walk never reached
    // are excluded.
    let excluded = boundary_parents
        .into_iter()
        .filter(|parent| !min_depth.contains_key(parent))
        .collect::<HashSet<_>>();

    let client: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let boundary_set: HashSet<ObjectId> = boundary.iter().copied().collect();
    let mut shallow_info = Vec::new();
    for oid in &boundary {
        if !client.contains(oid) {
            shallow_info.push(ProtocolV2FetchShallowInfo::Shallow(*oid));
        }
    }
    let mut extra_wants = Vec::new();
    for oid in &client_shallow {
        // A client-shallow commit is unshallowed when the walk reached it as
        // a non-boundary commit (upstream `send_unshallow`: NOT_SHALLOW set).
        let unshallowed = min_depth.contains_key(oid) && !boundary_set.contains(oid);
        if !unshallowed {
            continue;
        }
        shallow_info.push(ProtocolV2FetchShallowInfo::Unshallow(*oid));
        let object = remote_db.read_object(oid)?;
        extra_wants.extend(sley_odb::grafted_parents(
            remote_db,
            oid,
            Commit::parse_ref(format, &object.body)?.parents,
        ));
    }
    Ok(LocalDeepenPlan {
        depth,
        deepen_since: false,
        deepen_not: 0,
        client_shallow,
        shallow_info,
        excluded,
        extra_wants,
    })
}

/// Upstream `INFINITE_DEPTH`: `--unshallow`, and the implicit deepen a shallow
/// server runs for a plain fetch so its graft points reach the client.
pub const INFINITE_DEPTH: u32 = 0x7fff_ffff;

/// Upstream `get_shallows_depth`: the minimum depth (head = 1) at which the
/// walk from `heads` meets one of the client's shallow points, or 0 when it
/// never does. Used to make `--deepen=N` relative to the current boundary.
fn client_shallow_min_depth<R: ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    heads: &[ObjectId],
    client_shallow: &[ObjectId],
) -> Result<u32> {
    if client_shallow.is_empty() {
        return Ok(0);
    }
    let client: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let mut min_depth: HashMap<ObjectId, u32> = HashMap::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for head in heads {
        let Some(commit) = peel_to_commit(remote_db, format, head)? else {
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(entry) = min_depth.entry(commit) {
            entry.insert(1);
            queue.push_back(commit);
        }
    }
    let mut best: u32 = 0;
    while let Some(oid) = queue.pop_front() {
        let commit_depth = min_depth[&oid];
        if client.contains(&oid) && (best == 0 || commit_depth < best) {
            best = commit_depth;
        }
        let object = remote_db.read_object(&oid)?;
        let parents = sley_odb::grafted_parents(
            remote_db,
            &oid,
            Commit::parse_ref(format, &object.body)?.parents,
        );
        for parent in parents {
            if let std::collections::hash_map::Entry::Vacant(entry) = min_depth.entry(parent) {
                entry.insert(commit_depth + 1);
                queue.push_back(parent);
            }
        }
    }
    Ok(best)
}

/// Deepen plan for the rev-list modes (`--shallow-since`, `--shallow-exclude`),
/// mirroring upstream `get_shallow_commits_by_rev_list`: the kept set is every
/// commit reachable from `heads` that is newer than `since` (when given) and
/// not reachable from a `deepen_not` tip; the boundary is every kept commit
/// with at least one parent outside the kept set.
pub fn compute_local_deepen_by_rev_list<R: ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    heads: &[ObjectId],
    client_shallow: Vec<ObjectId>,
    since: Option<i64>,
    deepen_not: &[ObjectId],
) -> Result<LocalDeepenPlan> {
    // Closure of the deepen-not tips (commits to subtract from the kept set).
    let mut excluded_not: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for tip in deepen_not {
        if let Some(commit) = peel_to_commit(remote_db, format, tip)?
            && excluded_not.insert(commit)
        {
            queue.push_back(commit);
        }
    }
    while let Some(oid) = queue.pop_front() {
        let object = remote_db.read_object(&oid)?;
        for parent in sley_odb::grafted_parents(
            remote_db,
            &oid,
            Commit::parse_ref(format, &object.body)?.parents,
        ) {
            if excluded_not.insert(parent) {
                queue.push_back(parent);
            }
        }
    }

    let commit_time = |oid: &ObjectId| -> Result<i64> {
        let object = remote_db.read_object(oid)?;
        Ok(Commit::parse_ref(format, &object.body)?
            .committer_signature()
            .map(|signature| signature.time.seconds)
            .unwrap_or(0))
    };
    let keeps = |oid: &ObjectId| -> Result<bool> {
        if excluded_not.contains(oid) {
            return Ok(false);
        }
        match since {
            Some(since) => Ok(commit_time(oid)? >= since),
            None => Ok(true),
        }
    };

    // Kept-set walk: only kept commits are expanded, so the walk never reads
    // objects past the cut (and stops at server graft points via the seam).
    let mut kept: HashSet<ObjectId> = HashSet::new();
    let mut kept_order: Vec<ObjectId> = Vec::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::new();
    for head in heads {
        let Some(commit) = peel_to_commit(remote_db, format, head)? else {
            continue;
        };
        if keeps(&commit)? && kept.insert(commit) {
            kept_order.push(commit);
            queue.push_back(commit);
        }
    }
    while let Some(oid) = queue.pop_front() {
        let object = remote_db.read_object(&oid)?;
        for parent in sley_odb::grafted_parents(
            remote_db,
            &oid,
            Commit::parse_ref(format, &object.body)?.parents,
        ) {
            if !kept.contains(&parent) && keeps(&parent)? {
                kept.insert(parent);
                kept_order.push(parent);
                queue.push_back(parent);
            }
        }
    }
    if kept.is_empty() {
        // Upstream `get_shallow_commits_by_rev_list` dies here.
        return Err(GitError::Command(
            "no commits selected for shallow requests".into(),
        ));
    }

    // Boundary: kept commits with a parent outside the kept set.
    let mut boundary = Vec::new();
    let mut boundary_set: HashSet<ObjectId> = HashSet::new();
    let mut excluded: HashSet<ObjectId> = HashSet::new();
    for oid in &kept_order {
        let object = remote_db.read_object(oid)?;
        let parents = sley_odb::grafted_parents(
            remote_db,
            oid,
            Commit::parse_ref(format, &object.body)?.parents,
        );
        let mut is_boundary = false;
        for parent in parents {
            if !kept.contains(&parent) {
                is_boundary = true;
                excluded.insert(parent);
            }
        }
        if is_boundary && boundary_set.insert(*oid) {
            boundary.push(*oid);
        }
    }

    let client: HashSet<ObjectId> = client_shallow.iter().copied().collect();
    let mut shallow_info = Vec::new();
    for oid in &boundary {
        if !client.contains(oid) {
            shallow_info.push(ProtocolV2FetchShallowInfo::Shallow(*oid));
        }
    }
    let mut extra_wants = Vec::new();
    for oid in &client_shallow {
        let unshallowed = kept.contains(oid) && !boundary_set.contains(oid);
        if !unshallowed {
            continue;
        }
        shallow_info.push(ProtocolV2FetchShallowInfo::Unshallow(*oid));
        let object = remote_db.read_object(oid)?;
        extra_wants.extend(sley_odb::grafted_parents(
            remote_db,
            oid,
            Commit::parse_ref(format, &object.body)?.parents,
        ));
    }
    Ok(LocalDeepenPlan {
        depth: 0,
        deepen_since: since.is_some(),
        deepen_not: deepen_not.len(),
        client_shallow,
        shallow_info,
        excluded,
        extra_wants,
    })
}

/// Fetch `wants` from a local repository at `remote_git_dir` into the repository
/// at `git_dir`, round-tripping the request and response through the protocol
/// codecs into the in-process upload-pack so the local path exercises the same
/// wire format as the networked transports. Objects already present locally are
/// skipped; `promisor` selects promisor-pack installation.
///
/// When `deepen` carries a [`LocalDeepenPlan`] (computed by the caller from the
/// primary planned tips via [`compute_local_deepen`]), the fetch is shallow: the
/// request replays the client's boundary as `shallow` lines plus a `deepen`
/// line, the pack walk stops at the plan's boundary, and the returned
/// shallow-info updates must be folded into `$GIT_DIR/shallow` (see
/// [`crate::apply_shallow_info`]). Empty for a full fetch.
#[allow(clippy::too_many_arguments)]
pub fn install_fetch_pack_via_local_upload_pack(
    git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    wants: Vec<ObjectId>,
    deepen: Option<&LocalDeepenPlan>,
    promisor: bool,
    record_promisor_refs: bool,
    filter: Option<sley_odb::PackObjectFilter>,
    refetch: bool,
    unpack_limit: Option<usize>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    let all_wants_present = wants
        .iter()
        .map(|want| local_db.contains(want))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .all(|contains| contains);
    let deepen_noop = match deepen {
        Some(plan) => plan.shallow_info.is_empty() && plan.extra_wants.is_empty(),
        None => true,
    };
    if all_wants_present && deepen_noop && !refetch {
        return Ok(Vec::new());
    }

    let request = UploadPackRequest {
        wants,
        // The `shallow` capability accompanies a deepen request on the wire
        // (mirrors the SSH path); a plain fetch keeps its existing wire form.
        capabilities: deepen
            .map(|_| {
                vec![Capability {
                    name: "shallow".into(),
                    value: None,
                }]
            })
            .unwrap_or_default(),
        shallow: deepen
            .map(|plan| plan.client_shallow.clone())
            .unwrap_or_default(),
        deepen: deepen.and_then(|plan| (plan.depth > 0).then_some(plan.depth)),
        ..UploadPackRequest::default()
    };
    let mut encoded_request = Vec::new();
    write_upload_pack_request(&mut encoded_request, Some(&request))?;
    let decoded_request = read_upload_pack_request(format, &mut encoded_request.as_slice())?
        .ok_or_else(|| GitError::InvalidFormat("encoded upload-pack request was empty".into()))?;

    let haves = if refetch {
        Vec::new()
    } else {
        local_have_oids(git_dir, format)?
    };
    let negotiation = UploadPackNegotiationRequest { haves, done: true };
    let mut encoded_negotiation = Vec::new();
    write_upload_pack_negotiation_request(&mut encoded_negotiation, &negotiation)?;
    let decoded_negotiation =
        read_upload_pack_negotiation_request(format, &mut encoded_negotiation.as_slice())?;

    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    for want in &decoded_request.wants {
        if !remote_db.contains(want)? {
            return Err(GitError::InvalidObject(format!(
                "upload-pack requested missing object {want}"
            )));
        }
    }
    let known_haves = decoded_negotiation
        .haves
        .into_iter()
        .filter_map(|oid| match remote_db.contains(&oid) {
            Ok(true) => Some(Ok(oid)),
            Ok(false) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<_>>>()?;
    // Trace2 `fetch-info` parity: upstream upload-pack emits a data_json
    // event the shallow tests grep for; the in-process server inherits the
    // client's GIT_TRACE2_EVENT just like a spawned upload-pack would.
    trace2_fetch_info(
        known_haves.len(),
        decoded_request.wants.len(),
        deepen.map(|plan| plan.depth).unwrap_or(0),
        deepen.map(|plan| plan.client_shallow.len()).unwrap_or(0),
        deepen.is_some_and(|plan| plan.deepen_since),
        deepen.map(|plan| plan.deepen_not).unwrap_or(0),
        filter.as_ref(),
    );
    // With a deepen plan the haves walk is cut at the client's existing
    // boundary: having a commit inside the old shallow window must not imply
    // having the history below it (upstream runs pack-objects with the
    // client's shallow file for exactly this reason).
    let mut excluded = match deepen {
        Some(plan) => {
            let cut: HashSet<ObjectId> = plan.client_shallow.iter().copied().collect();
            sley_odb::collect_reachable_object_ids_with_cut(&remote_db, format, known_haves, &cut)?
        }
        None => collect_reachable_object_ids(&remote_db, format, known_haves)?,
    };
    let mut starts = decoded_request.wants;
    let promisor_ref_wants = starts.iter().copied().collect::<HashSet<_>>();
    for want in &starts {
        excluded.remove(want);
    }
    if let Some(plan) = deepen {
        // Stop the pack walk at the shallow boundary and pack the history a
        // moved boundary newly exposes.
        excluded.extend(plan.excluded.iter().copied());
        starts.extend(plan.extra_wants.iter().copied());
    }
    let install = build_and_install_reachable_pack_filtered(
        &remote_db,
        &local_db,
        format,
        starts,
        &excluded,
        RawPackInstallOptions { promisor },
        filter.clone(),
        unpack_limit,
    )?;
    if promisor
        && record_promisor_refs
        && let Some(result) = install
        && let Some(promisor_path) = result.promisor_path
    {
        append_promisor_ref_lines(&promisor_path, remote_git_dir, format, &promisor_ref_wants)?;
    }
    Ok(deepen
        .map(|plan| plan.shallow_info.clone())
        .unwrap_or_default())
}

fn append_promisor_ref_lines(
    promisor_path: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    wanted: &HashSet<ObjectId>,
) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let store = FileRefStore::new(remote_git_dir, format);
    let mut lines = Vec::new();
    if let Some(head_target) = store.read_ref("HEAD")? {
        let head = Ref {
            name: "HEAD".into(),
            target: head_target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &head)?
            && wanted.contains(&oid)
        {
            lines.push(format!("{oid} HEAD\n"));
        }
    }
    for reference in store.list_refs()? {
        let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        if wanted.contains(&oid) {
            lines.push(format!("{oid} {}\n", reference.name));
        }
    }
    if lines.is_empty() {
        return Ok(());
    }
    lines.sort();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(promisor_path)?;
    use std::io::Write as _;
    for line in lines {
        file.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Append upstream upload-pack's `fetch-info` data_json event to the file
/// named by `GIT_TRACE2_EVENT` (`trace2_fetch_info` in `upload-pack.c`). The
/// subset of fields the test suite greps is emitted with upstream spellings.
fn trace2_fetch_info(
    haves: usize,
    wants: usize,
    depth: u32,
    shallows: usize,
    deepen_since: bool,
    deepen_not: usize,
    filter: Option<&sley_odb::PackObjectFilter>,
) {
    let Some(path) = std::env::var_os("GIT_TRACE2_EVENT") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let filter_json = match filter {
        Some(sley_odb::PackObjectFilter::BlobNone) => "\"blob:none\"".to_string(),
        Some(sley_odb::PackObjectFilter::BlobLimit(limit)) => {
            format!("\"blob:limit={limit}\"")
        }
        Some(sley_odb::PackObjectFilter::TreeDepth(depth)) => {
            format!("\"tree:{depth}\"")
        }
        Some(sley_odb::PackObjectFilter::SparsePathSet(_)) => "\"sparse:oid\"".to_string(),
        None => "null".to_string(),
    };
    let line = format!(
        "{{\"event\":\"data_json\",\"thread\":\"main\",\"category\":\"upload-pack\",\"key\":\"fetch-info\",\"value\":{{\"haves\":{haves},\"wants\":{wants},\"want-refs\":0,\"depth\":{depth},\"shallows\":{shallows},\"deepen-since\":{deepen_since},\"deepen-not\":{deepen_not},\"deepen-relative\":false,\"filter\":{filter_json}}}}}\n"
    );
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write as _;
        let _ = file.write_all(line.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Protocol v2 upload-pack server (`GIT_PROTOCOL=version=2`).
//
// Mirrors upstream `upload-pack.c::upload_pack_v2` / `serve.c`: advertise the
// v2 capabilities, then read `command=ls-refs` / `command=fetch` requests until
// EOF, answering each with the protocol-v2 response. The transport (file://
// spawned process, git:// daemon child) hands us a connected stdin/stdout pair;
// everything below is transport-independent.
// ---------------------------------------------------------------------------

/// The v2 capabilities advertised by the upload-pack server, in the order git
/// emits them: `agent`, `ls-refs=unborn`, `fetch=shallow wait-for-done`,
/// `server-option`, `object-format=<hash>`.
fn upload_pack_v2_capabilities(format: ObjectFormat) -> Vec<Capability> {
    vec![
        Capability {
            name: "agent".into(),
            value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
        },
        Capability {
            name: "ls-refs".into(),
            value: Some("unborn".into()),
        },
        Capability {
            name: "fetch".into(),
            value: Some("shallow wait-for-done".into()),
        },
        Capability {
            name: "server-option".into(),
            value: None,
        },
        Capability {
            name: "object-format".into(),
            value: Some(format.name().into()),
        },
    ]
}

/// Resolve the symref target of `HEAD` (e.g. `refs/heads/main`) for the
/// `symrefs`/symref-target ls-refs attribute, following one level of symbolic
/// indirection. Returns `None` for a detached or missing `HEAD`.
fn head_symref_target(store: &FileRefStore) -> Result<Option<String>> {
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => Ok(Some(name)),
        _ => Ok(None),
    }
}

/// Build the protocol-v2 `ls-refs` records for the repository at `git_dir`,
/// honoring the request's `ref-prefix`, `peel`, `symrefs`, and `unborn`
/// arguments. Mirrors `ls-refs.c::ls_refs`.
fn local_ls_refs_v2_records(
    git_dir: &Path,
    format: ObjectFormat,
    request: &ProtocolV2LsRefsRequest,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let store = FileRefStore::new(git_dir, format);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_symref = head_symref_target(&store)?;

    // Build the (name -> oid, symref) list in git's advertisement order: HEAD
    // first (when present), then the sorted ref list from `for-each-ref`.
    let mut entries: Vec<(String, ObjectId, Option<String>)> = Vec::new();
    if let Some(target) = store.read_ref("HEAD")? {
        let reference = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? {
            entries.push(("HEAD".to_string(), oid, head_symref.clone()));
        } else if request.unborn {
            // An unborn HEAD (points at a not-yet-created branch) is reported as
            // an `unborn` record carrying its symref-target.
            entries.push(("HEAD".to_string(), ObjectId::null(format), head_symref.clone()));
        }
    }
    for reference in store.list_refs()? {
        let name = reference.name.clone();
        let Some((oid, symref)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        entries.push((name, oid, symref));
    }

    let matches_prefix = |name: &str| -> bool {
        if request.ref_prefixes.is_empty() {
            return true;
        }
        request
            .ref_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
    };

    let mut records = Vec::new();
    for (name, oid, symref) in entries {
        if !matches_prefix(&name) {
            continue;
        }
        // Unborn HEAD: only the all-zero placeholder reaches here with `unborn`.
        if name == "HEAD" && oid == ObjectId::null(format) {
            records.push(ProtocolV2LsRefsRecord::Unborn {
                name,
                symref_target: if request.symrefs { symref } else { None },
                attributes: Vec::new(),
            });
            continue;
        }
        let peeled = if request.peel {
            let object = db.read_object(&oid)?;
            if object.object_type == ObjectType::Tag {
                Some(sley_rev::peel_tags(&db, format, &oid)?)
            } else {
                None
            }
        } else {
            None
        };
        let symref_target = if request.symrefs { symref } else { None };
        records.push(ProtocolV2LsRefsRecord::Ref(ProtocolV2LsRefsRef {
            oid,
            name,
            peeled,
            symref_target,
            attributes: Vec::new(),
        }));
    }
    Ok(records)
}

/// Chunk a raw packfile into sideband channel-1 (`SideBandChannel::Data`)
/// pkt-lines for the v2 fetch `packfile` section, matching the upstream
/// `0001`-prefixed framing. Each chunk carries at most
/// `PKT_LINE_MAX_PAYLOAD_LEN - 1` packfile bytes (the leading byte is the
/// channel marker).
fn packfile_section_lines(pack: &[u8]) -> Vec<Vec<u8>> {
    let chunk = PKT_LINE_MAX_PAYLOAD_LEN - 1;
    let mut lines = Vec::new();
    for slice in pack.chunks(chunk) {
        let mut payload = Vec::with_capacity(slice.len() + 1);
        payload.push(1u8); // SideBandChannel::Data
        payload.extend_from_slice(slice);
        lines.push(payload);
    }
    lines
}

/// Build the protocol-v2 `fetch` response sections for a request against the
/// repository at `git_dir`. Mirrors `upload-pack.c::upload_pack_v2`'s
/// stateless single-round behavior: the client always sends `done` (the v2
/// clone/fetch path negotiates haves up front and finishes with `done`), so the
/// acknowledgments section is omitted and the response is just the packfile.
fn local_fetch_v2_sections(
    git_dir: &Path,
    format: ObjectFormat,
    request: &ProtocolV2FetchRequest,
) -> Result<Vec<ProtocolV2FetchResponseSection>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);

    let mut sections = Vec::new();

    // Acknowledgments: per gitprotocol-v2, when the client sends `done` the
    // acknowledgments section MUST be omitted. Without `done` (multi-round
    // negotiation) we answer NAK/ACK for the haves we have in common; the v2
    // file:// client always finishes with `done` so this branch is the
    // negotiation fallback.
    if !request.done {
        let mut acks: Vec<ProtocolV2FetchAcknowledgment> = Vec::new();
        for have in &request.haves {
            if db.contains(have)? {
                acks.push(ProtocolV2FetchAcknowledgment::Ack(*have));
            }
        }
        if acks.is_empty() {
            acks.push(ProtocolV2FetchAcknowledgment::Nak);
        }
        sections.push(ProtocolV2FetchResponseSection::Acknowledgments(acks));
        // Without `done` and no `ready`, the server stops here to let the
        // client continue negotiating; it would re-issue fetch with `done`.
        if !request.wait_for_done {
            return Ok(sections);
        }
    }

    // Wanted-refs: resolve each `want-ref <name>` to its current oid.
    if !request.want_refs.is_empty() {
        let store = FileRefStore::new(git_dir, format);
        let mut wanted = Vec::new();
        for name in &request.want_refs {
            let reference = Ref {
                name: name.clone(),
                target: store
                    .read_ref(name)?
                    .ok_or_else(|| GitError::not_found(format!("want-ref {name}")))?,
            };
            let (oid, _) = resolve_for_each_ref_target(&store, &reference)?
                .ok_or_else(|| GitError::not_found(format!("want-ref {name}")))?;
            wanted.push(sley_protocol::ProtocolV2FetchWantedRef {
                oid,
                name: name.clone(),
            });
        }
        sections.push(ProtocolV2FetchResponseSection::WantedRefs(wanted));
    }

    // Resolve want-refs into concrete wants for the pack walk.
    let mut wants: Vec<ObjectId> = request.wants.clone();
    if !request.want_refs.is_empty()
        && let Some(ProtocolV2FetchResponseSection::WantedRefs(wanted)) = sections
            .iter()
            .find(|s| matches!(s, ProtocolV2FetchResponseSection::WantedRefs(_)))
    {
        for w in wanted {
            wants.push(w.oid);
        }
    }

    // Packfile section: build the reachable pack excluding the client's haves.
    let mut known_haves: Vec<ObjectId> = Vec::new();
    for have in &request.haves {
        if db.contains(have)? {
            known_haves.push(*have);
        }
    }
    let excluded = collect_reachable_object_ids(&db, format, known_haves)?;
    let pack = build_reachable_pack(&db, format, wants, &excluded)?
        .map(|pack| pack.pack)
        .unwrap_or_default();

    sections.push(ProtocolV2FetchResponseSection::Packfile(
        packfile_section_lines(&pack),
    ));
    Ok(sections)
}

/// Serve a protocol-v2 upload-pack session over `reader`/`writer` for the
/// repository at `git_dir`. Writes the capability advertisement, then loops
/// reading `command=` requests (`ls-refs` / `fetch`) until the client closes
/// the connection (EOF). Mirrors `upload-pack.c::upload_pack_v2` driven by
/// `serve.c`.
pub fn serve_upload_pack_v2(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: upload_pack_v2_capabilities(format),
    };
    write_protocol_v2_advertisement(writer, &handshake)?;
    writer.flush()?;

    // EOF / a lone flush after the advertisement ends the session: the client
    // disconnected (e.g. `ls-remote` reads the refs and leaves). Malformed
    // requests after a command line are protocol violations and must fail
    // visibly instead of being treated as a clean disconnect.
    loop {
        let request = match read_protocol_v2_command_request(reader) {
            Ok(request) => request,
            Err(GitError::InvalidFormat(message))
                if message == "pkt-line stream ended before control packet"
                    || message == "protocol v2 command request must start with a command line" =>
            {
                break;
            }
            Err(err) => return Err(err),
        };
        match request.command.as_str() {
            "ls-refs" => {
                let ls_refs = ProtocolV2LsRefsRequest::from_command_request(&request)?;
                let records = local_ls_refs_v2_records(git_dir, format, &ls_refs)?;
                write_protocol_v2_ls_refs_response(writer, &records)?;
                writer.flush()?;
            }
            "fetch" => {
                let fetch = ProtocolV2FetchRequest::from_command_request(format, &request)?;
                let sections = local_fetch_v2_sections(git_dir, format, &fetch)?;
                write_protocol_v2_fetch_response(writer, &sections)?;
                writer.flush()?;
            }
            other => {
                return Err(GitError::InvalidFormat(format!(
                    "unsupported protocol v2 command {other}"
                )));
            }
        }
    }
    Ok(())
}
