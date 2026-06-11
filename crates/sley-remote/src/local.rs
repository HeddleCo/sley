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
use std::path::Path;

use sley_core::{Capability, GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType, Tag};
use sley_odb::{
    FileObjectDatabase, ObjectReader, RawPackInstallOptions, build_and_install_reachable_pack,
    build_reachable_pack, collect_reachable_object_ids,
};
use sley_protocol::{
    PKT_LINE_MAX_PAYLOAD_LEN, ProtocolV2FetchShallowInfo, ReceivePackFeatures,
    ReceivePackPushRequest, ReceivePackReportStatus, ReceivePackRequest, RefAdvertisement,
    SideBandChannel, SideBandPacket, UploadPackFeatures, UploadPackNegotiationRequest,
    UploadPackPackfileResponse, UploadPackRawPackfileResponse, UploadPackRequest,
    apply_receive_pack_push_request, build_upload_pack_raw_packfile_response,
    encode_receive_pack_features, encode_upload_pack_features,
    read_upload_pack_negotiation_request, read_upload_pack_request,
    write_upload_pack_negotiation_request, write_upload_pack_request,
};
use sley_refs::{FileRefStore, Ref, RefTarget, RefUpdate};

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
            for command in commands {
                tx.update(RefUpdate {
                    name: command.name.clone(),
                    expected: (!command.old_id.is_null())
                        .then(|| RefTarget::Direct(command.old_id.clone())),
                    new: RefTarget::Direct(command.new_id.clone()),
                    reflog: None,
                });
            }
            tx.commit()
        },
        |name| remote_store.delete_ref(name).map(|_| ()),
    )
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
            for command in commands {
                tx.update(RefUpdate {
                    name: command.name.clone(),
                    expected: (!command.old_id.is_null())
                        .then(|| RefTarget::Direct(command.old_id.clone())),
                    new: RefTarget::Direct(command.new_id.clone()),
                    reflog: None,
                });
            }
            tx.commit()
        },
        |name| remote_store.delete_ref(name).map(|_| ()),
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
    /// The requested deepen depth (`--depth N`, always >= 1).
    pub depth: u32,
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
) -> Result<LocalDeepenPlan> {
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
        let parents = Commit::parse_ref(format, &object.body)?.parents;
        if commit_depth + 1 >= depth {
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
    let mut shallow_info = Vec::new();
    for oid in &boundary {
        if !client.contains(oid) {
            shallow_info.push(ProtocolV2FetchShallowInfo::Shallow(*oid));
        }
    }
    let mut extra_wants = Vec::new();
    for oid in &client_shallow {
        let unshallowed = min_depth.get(oid).is_some_and(|d| d + 1 < depth);
        if !unshallowed {
            continue;
        }
        shallow_info.push(ProtocolV2FetchShallowInfo::Unshallow(*oid));
        let object = remote_db.read_object(oid)?;
        extra_wants.extend(Commit::parse_ref(format, &object.body)?.parents);
    }
    Ok(LocalDeepenPlan {
        depth,
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
pub fn install_fetch_pack_via_local_upload_pack(
    git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    wants: Vec<ObjectId>,
    deepen: Option<&LocalDeepenPlan>,
    promisor: bool,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    if wants.is_empty() {
        return Ok(Vec::new());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    // A deepen request must always run: even when every want is already present
    // the shallow boundary may move (mirrors the SSH path).
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
        deepen: deepen.map(|plan| plan.depth),
        ..UploadPackRequest::default()
    };
    let mut encoded_request = Vec::new();
    write_upload_pack_request(&mut encoded_request, Some(&request))?;
    let decoded_request = read_upload_pack_request(format, &mut encoded_request.as_slice())?
        .ok_or_else(|| GitError::InvalidFormat("encoded upload-pack request was empty".into()))?;

    let haves = local_have_oids(git_dir, format)?;
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
    let mut excluded = collect_reachable_object_ids(&remote_db, format, known_haves)?;
    let mut starts = decoded_request.wants;
    if let Some(plan) = deepen {
        // Stop the pack walk at the shallow boundary and pack the history a
        // moved boundary newly exposes.
        excluded.extend(plan.excluded.iter().copied());
        starts.extend(plan.extra_wants.iter().copied());
    }
    build_and_install_reachable_pack(
        &remote_db,
        &local_db,
        format,
        starts,
        &excluded,
        RawPackInstallOptions { promisor },
    )?;
    Ok(deepen
        .map(|plan| plan.shallow_info.clone())
        .unwrap_or_default())
}
