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

use std::collections::HashSet;
use std::path::Path;

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_fetch::{install_upload_pack_raw_promisor_response, install_upload_pack_raw_response};
use sley_odb::{build_reachable_pack, collect_reachable_object_ids, FileObjectDatabase};
use sley_protocol::{
    apply_receive_pack_push_request, build_upload_pack_raw_packfile_response,
    encode_receive_pack_features, encode_upload_pack_features,
    read_upload_pack_negotiation_request, read_upload_pack_raw_packfile_response,
    read_upload_pack_request, write_upload_pack_negotiation_request,
    write_upload_pack_raw_packfile_response, write_upload_pack_request, ReceivePackFeatures,
    ReceivePackPushRequest, ReceivePackReportStatus, ReceivePackRequest, RefAdvertisement,
    SideBandChannel, SideBandPacket, UploadPackFeatures, UploadPackNegotiationRequest,
    UploadPackPackfileResponse, UploadPackRawPackfileResponse, UploadPackRequest,
    PKT_LINE_MAX_PAYLOAD_LEN,
};
use sley_refs::{FileRefStore, Ref, RefTarget, RefUpdate};

/// The all-zero object id for `format`, used for the synthetic
/// `capabilities^{}` advertisement when a repository has no refs.
fn zero_oid(format: ObjectFormat) -> Result<ObjectId> {
    ObjectId::from_raw(format, &vec![0; format.raw_len()])
}

/// Whether `oid` is the all-zero object id (a ref creation/deletion sentinel).
fn is_zero_object_id(oid: &ObjectId) -> bool {
    oid.as_bytes().iter().all(|byte| *byte == 0)
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
                    expected: (!is_zero_object_id(&command.old_id))
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
        if seen.insert(advertisement.oid.clone()) {
            haves.push(advertisement.oid);
        }
    }
    Ok(haves)
}

/// Fetch `wants` from a local repository at `remote_git_dir` into the repository
/// at `git_dir`, round-tripping the request and response through the protocol
/// codecs into the in-process upload-pack so the local path exercises the same
/// wire format as the networked transports. Objects already present locally are
/// skipped; `promisor` selects promisor-pack installation.
pub fn install_fetch_pack_via_local_upload_pack(
    git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
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

    let features = upload_pack_features(remote_git_dir, format)?;
    let request = UploadPackRequest {
        wants,
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

    let response = upload_pack_from_local_repository(
        remote_git_dir,
        format,
        &features,
        decoded_request,
        decoded_negotiation.haves.into_iter().collect(),
    )?;
    let mut encoded_response = Vec::new();
    write_upload_pack_raw_packfile_response(&mut encoded_response, &response)?;
    let decoded_response =
        read_upload_pack_raw_packfile_response(format, &mut encoded_response.as_slice())?;
    if promisor {
        install_upload_pack_raw_promisor_response(&decoded_response, &local_db)?;
    } else {
        install_upload_pack_raw_response(&decoded_response, &local_db)?;
    }
    Ok(())
}
