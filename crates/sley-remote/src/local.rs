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

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Cursor, ErrorKind, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sley_config::GitConfig;
use sley_core::{
    CancelFlag, Capability, GitError, ObjectFormat, ObjectId, Result, UPSTREAM_GIT_COMPAT_VERSION,
};
use sley_object::{Commit, ObjectType, Tag};
use sley_odb::{
    FileObjectDatabase, ObjectReader, RawPackInstallOptions, ReachablePackMissingPolicy,
    ReachablePackThinBaseCandidates, build_and_install_reachable_pack,
    build_and_install_reachable_pack_filtered_with_thin_bases, build_reachable_pack,
    build_reachable_pack_filtered, collect_reachable_object_ids,
};
use sley_protocol::{
    PKT_LINE_MAX_PAYLOAD_LEN, PktLineFrame, ProtocolV2FetchAcknowledgment, ProtocolV2FetchFeatures,
    ProtocolV2FetchRequest, ProtocolV2FetchResponseSection, ProtocolV2FetchShallowInfo,
    ProtocolV2FetchWantedRef, ProtocolV2LsRefsFeatures, ProtocolV2LsRefsRecord,
    ProtocolV2LsRefsRef, ProtocolV2LsRefsRequest, ProtocolVersion, ReceivePackCommand,
    ReceivePackCommandStatus, ReceivePackFeatures, ReceivePackPushRequest,
    ReceivePackPushRequestHeader, ReceivePackReportStatus, ReceivePackRequest,
    ReceivePackUnpackStatus, RefAdvertisement, SideBandChannel, SideBandPacket, TransportHandshake,
    UploadPackAckStatus, UploadPackAcknowledgment, UploadPackFeatures,
    UploadPackNegotiationRequest, UploadPackPackfileResponse, UploadPackRawPackfileResponse,
    UploadPackRequest, apply_receive_pack_push_request, build_upload_pack_raw_packfile_response,
    classify_protocol_v2_command_request, encode_protocol_v2_fetch_capability,
    encode_protocol_v2_ls_refs_capability, encode_receive_pack_features,
    encode_upload_pack_features, read_protocol_v2_command_request, read_upload_pack_acknowledgment,
    read_upload_pack_negotiation_request, read_upload_pack_request,
    validate_receive_pack_push_request_features, write_pkt_line_frame, write_pkt_line_payload,
    write_protocol_v2_advertisement, write_protocol_v2_fetch_request,
    write_protocol_v2_fetch_response, write_protocol_v2_ls_refs_response,
    write_upload_pack_acknowledgment, write_upload_pack_negotiation_request,
    write_upload_pack_request,
};
use sley_refs::{
    DeleteRef, FileRefStore, Ref, RefDeletePrecondition, RefPrecondition, RefTarget, ReflogEntry,
};

use crate::install::install_protocol_v2_fetch_response_from_reader_with_cancel;

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
    let head_name = sley_core::expand_namespace("HEAD");
    if let Some(RefTarget::Symbolic(target)) = store.read_ref(&head_name)? {
        // Advertise the logical (namespace-stripped) target so clients see the
        // same names they will later request.
        let logical_target = sley_core::strip_namespace(&target)
            .unwrap_or(target.as_str())
            .to_string();
        symrefs.push(format!("HEAD:{logical_target}"));
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
/// status, ref deletion, ofs-delta, push-options, quiet, no-thin, and the object
/// format.
pub fn receive_pack_features(format: ObjectFormat) -> ReceivePackFeatures {
    ReceivePackFeatures {
        report_status: true,
        report_status_v2: true,
        delete_refs: true,
        ofs_delta: true,
        push_options: true,
        quiet: true,
        no_thin: true,
        atomic: true,
        side_band_64k: true,
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
    let deletes_applied_with_updates = std::cell::RefCell::new(HashSet::<String>::new());
    apply_receive_pack_push_request(
        &receive_pack_features(format),
        request,
        |name| match remote_store.read_ref(name)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            Some(RefTarget::Symbolic(_)) | None => Ok(None),
        },
        |packfile| {
            let mut reader = packfile;
            remote_db
                .install_raw_pack_from_reader(&mut reader)
                .map(|_| ())
        },
        |oid| remote_db.contains(oid),
        |commands| {
            let applied = apply_receive_pack_ref_transaction(
                remote_git_dir,
                format,
                &remote_store,
                commands,
                &request.commands.commands,
            )?;
            deletes_applied_with_updates.borrow_mut().extend(applied);
            Ok(())
        },
        |command| {
            if deletes_applied_with_updates
                .borrow()
                .contains(command.name.as_str())
            {
                return Ok(());
            }
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

/// Apply a receive-pack push while streaming the optional incoming packfile from
/// `pack_reader` into the object database. This mirrors
/// [`receive_pack_into_local_repository`] but avoids materializing the pack as a
/// `Vec<u8>` in stdio/SSH server paths.
pub fn receive_pack_stream_into_local_repository<R: Read>(
    remote_git_dir: &Path,
    format: ObjectFormat,
    header: &ReceivePackPushRequestHeader,
    pack_reader: &mut R,
) -> Result<ReceivePackReportStatus> {
    let remote_store = FileRefStore::new(remote_git_dir, format);
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format);
    let pack_prefix = read_optional_pack_prefix(pack_reader)?;
    let validation_request = ReceivePackPushRequest {
        commands: header.commands.clone(),
        push_options: header.push_options.clone(),
        packfile: pack_prefix.clone().unwrap_or_default(),
    };
    validate_receive_pack_push_request_features(
        &receive_pack_features(format),
        &validation_request,
    )?;

    let deletes_applied_with_updates = std::cell::RefCell::new(HashSet::<String>::new());
    for command in header
        .commands
        .commands
        .iter()
        .filter(|command| command.new_id.is_null())
    {
        let current = match remote_store.read_ref(&command.name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            Some(RefTarget::Symbolic(_)) | None => None,
        };
        if !command.old_id.is_null() && current != Some(command.old_id.clone()) {
            return Err(GitError::Transaction(format!(
                "expected ref {} to match",
                command.name
            )));
        }
    }

    let updates = header
        .commands
        .commands
        .iter()
        .filter(|command| !command.new_id.is_null())
        .cloned()
        .collect::<Vec<_>>();
    if !updates.is_empty() {
        if let Some(prefix) = pack_prefix {
            let mut stream = Cursor::new(prefix).chain(pack_reader);
            remote_db
                .install_raw_pack_from_reader(&mut stream)
                .map(|_| ())?;
        }
        for command in &updates {
            if !remote_db.contains(&command.new_id)? {
                return Err(GitError::InvalidObject(format!(
                    "receive-pack packfile did not provide {}",
                    command.new_id
                )));
            }
        }
        let applied = apply_receive_pack_ref_transaction(
            remote_git_dir,
            format,
            &remote_store,
            &updates,
            &header.commands.commands,
        )?;
        deletes_applied_with_updates.borrow_mut().extend(applied);
    }

    for command in header
        .commands
        .commands
        .iter()
        .filter(|command| command.new_id.is_null())
    {
        if deletes_applied_with_updates
            .borrow()
            .contains(command.name.as_str())
        {
            continue;
        }
        remote_store
            .delete_ref_checked(DeleteRef {
                name: command.name.clone(),
                expected_old: (!command.old_id.is_null()).then_some(command.old_id),
                reflog: None,
            })
            .map(|_| ())
            .map_err(|err| GitError::Transaction(err.to_string()))?;
    }

    Ok(ReceivePackReportStatus {
        unpack: ReceivePackUnpackStatus::Ok,
        commands: header
            .commands
            .commands
            .iter()
            .map(|command| ReceivePackCommandStatus::Ok {
                name: command.name.clone(),
            })
            .collect(),
    })
}

fn read_optional_pack_prefix(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 4];
    loop {
        match reader.read(&mut prefix[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
    reader.read_exact(&mut prefix[1..])?;
    if &prefix != b"PACK" {
        return Err(GitError::InvalidFormat(
            "receive-pack packfile must start with PACK".into(),
        ));
    }
    Ok(Some(prefix.to_vec()))
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
    let deletes_applied_with_updates = std::cell::RefCell::new(HashSet::<String>::new());
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
                RawPackInstallOptions {
                    promisor: false,
                    ..Default::default()
                },
            )?;
            Ok(())
        },
        |oid| remote_db.contains(oid),
        |commands| {
            let applied = apply_receive_pack_ref_transaction(
                remote_git_dir,
                format,
                &remote_store,
                commands,
                &request.commands.commands,
            )?;
            deletes_applied_with_updates.borrow_mut().extend(applied);
            Ok(())
        },
        |command| {
            if deletes_applied_with_updates
                .borrow()
                .contains(command.name.as_str())
            {
                return Ok(());
            }
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

pub(crate) fn apply_receive_pack_ref_transaction(
    remote_git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    updates: &[ReceivePackCommand],
    all_commands: &[ReceivePackCommand],
) -> Result<HashSet<String>> {
    let updates = canonical_receive_pack_update_commands(store, updates)?;
    let deletes = all_commands
        .iter()
        .filter(|command| command.new_id.is_null())
        .collect::<Vec<_>>();
    let mut tx = store.transaction();
    for command in &deletes {
        tx.delete_with_precondition(
            command.name.clone(),
            RefDeletePrecondition::Direct((!command.old_id.is_null()).then_some(command.old_id)),
            None,
        );
    }
    let log_updates = receive_pack_log_all_ref_updates(remote_git_dir);
    for command in &updates {
        let precondition = if command.old_id.is_null() {
            RefPrecondition::MustNotExist
        } else {
            RefPrecondition::MustExistAndMatch(RefTarget::Direct(command.old_id))
        };
        let reflog = if log_updates && receive_pack_should_write_reflog(&command.name) {
            Some(receive_pack_reflog_entry(
                format,
                command.old_id,
                command.new_id,
            ))
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
    tx.commit()?;
    Ok(deletes
        .into_iter()
        .map(|command| command.name.clone())
        .collect())
}

fn canonical_receive_pack_update_commands(
    store: &FileRefStore,
    commands: &[ReceivePackCommand],
) -> Result<Vec<ReceivePackCommand>> {
    let mut by_actual = HashMap::<String, ObjectId>::new();
    let mut canonical = Vec::with_capacity(commands.len());
    for command in commands {
        let name = match store.read_ref(&command.name)? {
            Some(RefTarget::Symbolic(target)) => target,
            Some(RefTarget::Direct(_)) | None => command.name.clone(),
        };
        if let Some(existing) = by_actual.get(&name) {
            if existing != &command.new_id {
                return Err(GitError::Command("refusing inconsistent update".into()));
            }
        } else {
            by_actual.insert(name.clone(), command.new_id);
        }
        canonical.push(ReceivePackCommand {
            old_id: command.old_id,
            new_id: command.new_id,
            name,
        });
    }
    Ok(canonical)
}

/// The ref advertisements a local repository would send to a fetching client:
/// `HEAD` (if resolvable) followed by every ref, each resolved to its object id.
///
/// When `GIT_NAMESPACE` / `--namespace` is active, only refs under the
/// namespace are advertised (with the namespace prefix stripped). Hidden refs
/// (`transfer.hideRefs` / `uploadpack.hideRefs`) are omitted using git's
/// stripped-vs-full matching rules.
pub fn local_fetch_advertisements(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<RefAdvertisement>> {
    let store = FileRefStore::new_without_reference_backend_env(git_dir, format);
    let namespace = sley_core::get_git_namespace();
    let hidden = transfer_upload_hidden_ref_patterns(git_dir);
    let mut advertisements = Vec::new();

    let head_name = if namespace.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{namespace}HEAD")
    };
    if let Some(target) = store.read_ref(&head_name)? {
        let reference = Ref {
            name: head_name.clone(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? {
            let logical = if namespace.is_empty() {
                "HEAD".to_string()
            } else {
                "HEAD".to_string()
            };
            if !sley_core::ref_is_hidden(Some(logical.as_str()), &head_name, &hidden) {
                advertisements.push(RefAdvertisement {
                    oid,
                    name: logical,
                    capabilities: Vec::new(),
                });
            }
        }
    }
    for reference in store.list_refs()? {
        let physical = reference.name.clone();
        let logical = if namespace.is_empty() {
            Some(physical.as_str())
        } else {
            physical.strip_prefix(namespace.as_str())
        };
        let Some(logical) = logical else {
            continue;
        };
        // Namespaced HEAD lives under `refs/namespaces/.../HEAD` and would
        // otherwise appear twice (once from the special HEAD read above).
        if logical == "HEAD" {
            continue;
        }
        if sley_core::ref_is_hidden(Some(logical), &physical, &hidden) {
            continue;
        }
        let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        advertisements.push(RefAdvertisement {
            oid,
            name: logical.to_string(),
            capabilities: Vec::new(),
        });
    }
    Ok(advertisements)
}

/// Collect `transfer.hideRefs` + `uploadpack.hideRefs` patterns for the
/// upload-pack / ls-remote advertisement path.
fn transfer_upload_hidden_ref_patterns(git_dir: &Path) -> Vec<String> {
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut out = Vec::new();
    for section in &config.sections {
        if section.subsection.is_some() {
            continue;
        }
        if !section.name.eq_ignore_ascii_case("transfer")
            && !section.name.eq_ignore_ascii_case("uploadpack")
        {
            continue;
        }
        for entry in &section.entries {
            if entry.key.eq_ignore_ascii_case("hiderefs")
                && let Some(value) = entry.value.as_deref()
            {
                out.push(sley_core::trim_hidden_ref_pattern(value));
            }
        }
    }
    out
}

/// Collect `transfer.hideRefs` + `receive.hideRefs` patterns for receive-pack.
pub(crate) fn transfer_receive_hidden_ref_patterns(config: &GitConfig) -> Vec<String> {
    let mut out = Vec::new();
    for section in &config.sections {
        if section.subsection.is_some() {
            continue;
        }
        if !section.name.eq_ignore_ascii_case("transfer")
            && !section.name.eq_ignore_ascii_case("receive")
        {
            continue;
        }
        for entry in &section.entries {
            if entry.key.eq_ignore_ascii_case("hiderefs")
                && let Some(value) = entry.value.as_deref()
            {
                out.push(sley_core::trim_hidden_ref_pattern(value));
            }
        }
    }
    out
}

/// The object ids the local repository can offer as `have`s during negotiation.
/// Ref tips are offered first, then every object visible through the local
/// object database, including alternates recorded in `objects/info/alternates`.
pub fn local_have_oids(git_dir: &Path, format: ObjectFormat) -> Result<Vec<ObjectId>> {
    let mut seen = HashSet::new();
    let mut haves = Vec::new();
    for advertisement in local_fetch_advertisements(git_dir, format)? {
        if seen.insert(advertisement.oid) {
            haves.push(advertisement.oid);
        }
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    for oid in db.object_ids()? {
        if seen.insert(oid) {
            haves.push(oid);
        }
    }
    Ok(haves)
}

/// Commit haves ordered from advertised tips toward their ancestors for
/// protocol negotiation. Unlike [`local_have_oids`], this intentionally omits
/// trees and blobs: Git's negotiator advances through commit history in
/// bounded batches, allowing the server to distinguish an immediately common
/// tip from a common ancestor reached only in a later round.
pub(crate) fn local_negotiation_have_oids(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    local_negotiation_have_oids_stopping_at(git_dir, format, &HashSet::new())
}

/// Commit haves ordered like [`local_negotiation_have_oids`], but stop walking
/// behind commits the remote advertised as ref tips. The advertisement is a
/// promise that the server already has the tip and its reachable history, so
/// sending ancestors of that tip only wastes negotiation lines. This mirrors
/// fetch-pack's `mark_tips()` boundary for the default/consecutive negotiator.
fn local_negotiation_have_oids_pruned_by_remote_tips(
    git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
) -> Result<Vec<ObjectId>> {
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format)
        .with_promisor_remote_present(repo_has_promisor_remote(remote_git_dir));
    let mut advertised_commits = HashSet::new();
    for advertisement in local_fetch_advertisements(remote_git_dir, format)? {
        // A corrupt advertisement is diagnosed later when its ref is wanted.
        // It cannot establish a negotiation frontier because the server does
        // not actually own the advertised object.
        if !remote_db.contains(&advertisement.oid)? {
            continue;
        }
        if let Some(commit) =
            peel_to_commit_for_negotiation(&remote_db, format, &advertisement.oid)?
        {
            advertised_commits.insert(commit);
        }
    }
    local_negotiation_have_oids_stopping_at(git_dir, format, &advertised_commits)
}

fn local_negotiation_have_oids_stopping_at(
    git_dir: &Path,
    format: ObjectFormat,
    stop_commits: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format)
        .with_promisor_remote_present(repo_has_promisor_remote(git_dir));
    let mut seen = HashSet::new();
    let mut queue = BinaryHeap::new();
    let commit_time = |oid: &ObjectId| -> Result<i64> {
        let object = db.read_object(oid)?;
        Ok(Commit::parse_ref(format, &object.body)?
            .committer_signature()
            .map(|signature| signature.time.seconds)
            .unwrap_or(0))
    };
    for advertisement in local_fetch_advertisements(git_dir, format)? {
        if let Some(commit) = peel_to_commit_for_negotiation(&db, format, &advertisement.oid)?
            && seen.insert(commit)
        {
            queue.push((commit_time(&commit)?, commit));
        }
    }

    let mut haves = Vec::new();
    while let Some((_time, oid)) = queue.pop() {
        haves.push(oid);
        if stop_commits.contains(&oid) {
            continue;
        }
        let object = db.read_object(&oid)?;
        for parent in
            sley_odb::grafted_parents(&db, &oid, Commit::parse_ref(format, &object.body)?.parents)
        {
            if seen.insert(parent) {
                queue.push((commit_time(&parent)?, parent));
            }
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

/// Negotiation-frontier variant of [`peel_to_commit`]. An annotated tag may be
/// present in a promisor pack while its filtered target is legitimately absent.
/// Such a tag cannot establish a commit-have boundary, but it must not abort an
/// unrelated branch fetch. Missing objects not proven promised remain errors.
fn peel_to_commit_for_negotiation<R: ObjectReader>(
    remote_db: &R,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Option<ObjectId>> {
    let mut oid = *oid;
    loop {
        let object = match remote_db.read_object(&oid) {
            Ok(object) => object,
            Err(GitError::NotFound(_)) if remote_db.is_promised_object(&oid) => return Ok(None),
            Err(error) => return Err(error),
        };
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

const INITIAL_NEGOTIATION_BATCH: usize = 16;
const MAX_IN_VAIN: usize = 256;

#[derive(Debug, Default)]
struct LocalNegotiationOutcome {
    common_haves: Vec<ObjectId>,
    total_rounds: usize,
}

/// Run the in-process transport through protocol-v2 fetch-pack's multi-round
/// state. Haves are sent in exponentially growing batches; an ACK resets
/// `in_vain`, and the client only gives up after 256 further haves once an ACK
/// has been observed. Requests and responses round-trip through the upload-pack
/// codecs, so trace data reflects the wire-driven state rather than a count
/// inferred after the pack has already been selected.
fn negotiate_local_upload_pack(
    remote_db: &FileObjectDatabase,
    format: ObjectFormat,
    wants: &[ObjectId],
    haves: Vec<ObjectId>,
) -> Result<LocalNegotiationOutcome> {
    let wanted_commits = wants
        .iter()
        .map(|want| {
            if remote_db.contains(want)? {
                peel_to_commit(remote_db, format, want)
            } else {
                // Preserve the fetch path's typed "requested missing object"
                // diagnostic after negotiation; a corrupt advertisement cannot
                // participate in readiness.
                Ok(None)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let mut outcome = LocalNegotiationOutcome::default();
    let mut next_have = 0usize;
    let mut batch_size = INITIAL_NEGOTIATION_BATCH;
    let mut in_vain = 0usize;
    let mut seen_ack = false;
    let mut common = HashSet::new();

    loop {
        outcome.total_rounds += 1;
        let end = next_have.saturating_add(batch_size).min(haves.len());
        let batch = haves[next_have..end].to_vec();
        next_have = end;
        in_vain = in_vain.saturating_add(batch.len());
        let done = batch.is_empty() || (seen_ack && in_vain >= MAX_IN_VAIN);

        let mut request_bytes = Vec::new();
        write_upload_pack_negotiation_request(
            &mut request_bytes,
            &UploadPackNegotiationRequest { haves: batch, done },
        )?;
        let decoded = read_upload_pack_negotiation_request(format, &mut request_bytes.as_slice())?;
        if decoded.done {
            break;
        }

        let mut round_common = Vec::new();
        for oid in &decoded.haves {
            if remote_db.contains(oid)? && common.insert(*oid) {
                round_common.push(*oid);
            }
        }
        let ready = !round_common.is_empty()
            && common_covers_wanted_commits(remote_db, format, &wanted_commits, &common)?;
        let mut acknowledgments = if round_common.is_empty() {
            vec![UploadPackAcknowledgment::Nak]
        } else {
            round_common
                .iter()
                .enumerate()
                .map(|(index, oid)| UploadPackAcknowledgment::Ack {
                    oid: *oid,
                    status: Some(if ready && index + 1 == round_common.len() {
                        UploadPackAckStatus::Ready
                    } else {
                        UploadPackAckStatus::Common
                    }),
                })
                .collect::<Vec<_>>()
        };
        let mut response_bytes = Vec::new();
        for acknowledgment in &acknowledgments {
            write_upload_pack_acknowledgment(&mut response_bytes, acknowledgment)?;
        }
        acknowledgments.clear();
        let mut response = response_bytes.as_slice();
        while !response.is_empty() {
            acknowledgments.push(read_upload_pack_acknowledgment(format, &mut response)?);
        }
        let mut received_ready = false;
        for acknowledgment in acknowledgments {
            if let UploadPackAcknowledgment::Ack { oid, status } = acknowledgment {
                seen_ack = true;
                in_vain = 0;
                outcome.common_haves.push(oid);
                received_ready |= status == Some(UploadPackAckStatus::Ready);
            }
        }
        if received_ready {
            break;
        }
        batch_size = batch_size.saturating_mul(2);
    }
    Ok(outcome)
}

fn common_covers_wanted_commits(
    remote_db: &FileObjectDatabase,
    format: ObjectFormat,
    wanted_commits: &[Option<ObjectId>],
    common: &HashSet<ObjectId>,
) -> Result<bool> {
    for wanted in wanted_commits.iter().flatten() {
        let mut stack = vec![*wanted];
        let mut seen = HashSet::new();
        let mut covered = false;
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            if common.contains(&oid) {
                covered = true;
                break;
            }
            let object = remote_db.read_object(&oid)?;
            stack.extend(sley_odb::grafted_parents(
                remote_db,
                &oid,
                Commit::parse_ref(format, &object.body)?.parents,
            ));
        }
        if !covered {
            return Ok(false);
        }
    }
    Ok(true)
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
    custom_haves: Option<Vec<ObjectId>>,
    refetch: bool,
    unpack_limit: Option<usize>,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    install_fetch_pack_via_local_upload_pack_with_promisor_decision(
        git_dir,
        remote_git_dir,
        format,
        wants,
        deepen,
        promisor,
        record_promisor_refs,
        filter,
        custom_haves,
        refetch,
        unpack_limit,
        &crate::PromisorRemoteDecision::default(),
    )
}

/// Local upload-pack with the typed protocol-v2 promisor decision carried into
/// pack traversal. Accepted remotes allow promised gaps to be omitted; an empty
/// decision requires the server to hydrate those gaps from its configured
/// local/file promisors before constructing the transfer pack.
#[allow(clippy::too_many_arguments)]
pub fn install_fetch_pack_via_local_upload_pack_with_promisor_decision(
    git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    wants: Vec<ObjectId>,
    deepen: Option<&LocalDeepenPlan>,
    promisor: bool,
    record_promisor_refs: bool,
    filter: Option<sley_odb::PackObjectFilter>,
    custom_haves: Option<Vec<ObjectId>>,
    refetch: bool,
    unpack_limit: Option<usize>,
    promisor_decision: &crate::PromisorRemoteDecision,
) -> Result<Vec<ProtocolV2FetchShallowInfo>> {
    install_fetch_pack_via_local_upload_pack_with_promisor_decision_into(
        git_dir,
        git_dir,
        remote_git_dir,
        format,
        wants,
        deepen,
        promisor,
        record_promisor_refs,
        filter,
        custom_haves,
        refetch,
        unpack_limit,
        promisor_decision,
        LocalFetchPackRequestMode::ExactObjects,
    )
    .map(|outcome| outcome.shallow_info)
}

#[derive(Debug, Default)]
pub(crate) struct LocalFetchPackOutcome {
    pub shallow_info: Vec<ProtocolV2FetchShallowInfo>,
    pub object_count: usize,
    pub compression_count: usize,
    pub delta_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalFetchPackRequestMode {
    /// Traverse from the requested tips, applying any negotiated object filter.
    Traversal,
    /// Hydrate explicitly named objects, which must not be removed by the
    /// repository's ordinary partial-clone traversal filter.
    ExactObjects,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_fetch_pack_via_local_upload_pack_with_promisor_decision_into(
    git_dir: &Path,
    destination_git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    wants: Vec<ObjectId>,
    deepen: Option<&LocalDeepenPlan>,
    promisor: bool,
    record_promisor_refs: bool,
    filter: Option<sley_odb::PackObjectFilter>,
    custom_haves: Option<Vec<ObjectId>>,
    refetch: bool,
    unpack_limit: Option<usize>,
    promisor_decision: &crate::PromisorRemoteDecision,
    request_mode: LocalFetchPackRequestMode,
) -> Result<LocalFetchPackOutcome> {
    if wants.is_empty() {
        return Ok(LocalFetchPackOutcome::default());
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format)
        .with_promisor_remote_present(repo_has_promisor_remote(git_dir));
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
        sley_protocol::trace_packet_write_payload(b"0000");
        return Ok(LocalFetchPackOutcome::default());
    }

    // A lazy promisor request names the exact missing objects it needs. The
    // repository's partial-clone filter describes ordinary traversal, but it
    // must not remove an explicitly wanted blob from this direct response.
    let direct_promisor_object_fetch = promisor
        && deepen.is_none()
        && !record_promisor_refs
        && request_mode == LocalFetchPackRequestMode::ExactObjects;
    let transfer_filter = if direct_promisor_object_fetch {
        None
    } else {
        filter
    };
    let request = UploadPackRequest {
        wants,
        filter: transfer_filter
            .as_ref()
            .and_then(upload_pack_filter_protocol_spec),
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

    // Lazy promisor hydration asks for exact missing objects; negotiating local
    // haves would walk the partial client's intentionally-missing blobs.
    if direct_promisor_object_fetch && local_upload_pack_client_wants_v2(git_dir) {
        trace_local_upload_pack_v2_capabilities(remote_git_dir, format);
    }
    let use_default_haves = !refetch && !direct_promisor_object_fetch && custom_haves.is_none();
    let haves = if refetch || direct_promisor_object_fetch {
        Vec::new()
    } else if let Some(haves) = custom_haves {
        haves
    } else {
        local_negotiation_have_oids_pruned_by_remote_tips(git_dir, remote_git_dir, format)?
    };
    let remote_has_promisor = repo_has_promisor_remote(remote_git_dir);
    let remote_db = FileObjectDatabase::from_git_dir(remote_git_dir, format)
        .with_promisor_remote_present(remote_has_promisor);
    let negotiation =
        negotiate_local_upload_pack(&remote_db, format, &decoded_request.wants, haves)?;
    sley_core::trace2::data("negotiation_v2", "total_rounds", negotiation.total_rounds);
    let mut known_haves = negotiation.common_haves;
    if use_default_haves {
        // The local transport can cheaply retain the full destination-object
        // exclusion set without putting every loose object on the simulated
        // wire. The packet negotiation above stays commit-only and
        // advertisement-pruned; this internal augmentation preserves support
        // for fetching from an intentionally incomplete local repository when
        // the destination already owns its missing delta/base objects.
        let mut seen = known_haves.iter().copied().collect::<HashSet<_>>();
        for oid in local_have_oids(git_dir, format)? {
            if seen.insert(oid) && remote_db.contains(&oid)? {
                known_haves.push(oid);
            }
        }
    }
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
        transfer_filter.as_ref(),
    );
    // With a deepen plan the haves walk is cut at the client's existing
    // boundary: having a commit inside the old shallow window must not imply
    // having the history below it (upstream runs pack-objects with the
    // client's shallow file for exactly this reason).
    let mut excluded = match deepen {
        Some(plan) => {
            let cut: HashSet<ObjectId> = plan.client_shallow.iter().copied().collect();
            let mut excluded = sley_odb::collect_reachable_object_ids_with_cut(
                &remote_db,
                format,
                known_haves,
                &cut,
            )?;
            // A `shallow <oid>` request line also proves that the client owns
            // the boundary commit itself. It only withholds knowledge of that
            // commit's parents. Exclude the boundary object while retaining
            // the cut so a deepen response can still send newly exposed
            // history below it.
            excluded.extend(plan.client_shallow.iter().copied());
            excluded
        }
        None => {
            // The negotiated haves describe the client's object graph. A local
            // remote may be intentionally incomplete while the client has the
            // missing bases already, so walk the exclusion closure locally and
            // keep the actual pack source pinned to the remote below.
            sley_odb::collect_reachable_object_ids_tolerating_promised_missing(
                &local_db,
                format,
                known_haves,
            )?
        }
    };
    let mut starts = decoded_request.wants;
    let promisor_ref_wants = starts.iter().copied().collect::<HashSet<_>>();
    if deepen.is_some() {
        // Deepening names the current tip again so upload-pack can plan the
        // boundary, but a client that already owns that tip must not receive a
        // duplicate loose copy. Newly exposed parents are added below through
        // `extra_wants`; retain only genuinely missing primary tips here.
        let mut missing_starts = Vec::with_capacity(starts.len());
        for oid in starts {
            if !local_db.contains(&oid)? {
                missing_starts.push(oid);
            }
        }
        starts = missing_starts;
    }
    for want in &starts {
        excluded.remove(want);
    }
    if let Some(plan) = deepen {
        // Stop the pack walk at the shallow boundary and pack the history a
        // moved boundary newly exposes.
        excluded.extend(plan.excluded.iter().copied());
        starts.extend(plan.extra_wants.iter().copied());
    }
    if remote_has_promisor && promisor_decision.accepted.is_empty() {
        hydrate_reachable_promised_objects(remote_git_dir, &remote_db, format, &starts, &excluded)?;
    }
    for want in &starts {
        if !remote_db.contains(want)?
            && (promisor_decision.accepted.is_empty() || !remote_db.is_promised_object(want))
        {
            return Err(GitError::InvalidObject(format!(
                "upload-pack requested missing object {want}"
            )));
        }
    }
    let missing_policy = if promisor_decision.accepted.is_empty() {
        ReachablePackMissingPolicy::RequireComplete
    } else {
        ReachablePackMissingPolicy::OmitPromised
    };
    let destination_db = FileObjectDatabase::from_git_dir(destination_git_dir, format)
        .with_promisor_remote_present(repo_has_promisor_remote(git_dir));
    // A bitmap-assisted server knows the complete client-have closure cheaply.
    // Keep those objects excluded from the response, but expose a bounded set
    // to pack generation as external bases so buried old blobs can be reused in
    // a thin pack. Without bitmap traversal Git intentionally limits the have
    // walk and does not discover those deep bases.
    let thin_base_candidates = if local_upload_pack_uses_bitmaps(remote_git_dir) {
        ReachablePackThinBaseCandidates::from_object_ids(&excluded)
    } else {
        ReachablePackThinBaseCandidates::default()
    };
    let transfer = build_and_install_reachable_pack_filtered_with_thin_bases(
        &remote_db,
        &destination_db,
        format,
        starts,
        &excluded,
        RawPackInstallOptions {
            promisor,
            ..Default::default()
        },
        transfer_filter,
        unpack_limit,
        missing_policy,
        thin_base_candidates,
    )?;
    if promisor
        && record_promisor_refs
        && let Some(result) = transfer.install.as_ref()
        && let Some(promisor_path) = result.promisor_path.as_ref()
    {
        append_promisor_ref_lines(promisor_path, remote_git_dir, format, &promisor_ref_wants)?;
    }
    Ok(LocalFetchPackOutcome {
        shallow_info: deepen
            .map(|plan| plan.shallow_info.clone())
            .unwrap_or_default(),
        object_count: transfer.object_count,
        compression_count: transfer.compression_count,
        delta_count: transfer.delta_count,
    })
}

/// Hydrate the requested object IDs from configured local/file promisor
/// remotes, returning the subset that was installed.
///
/// Remotes are tried in [`crate::configured_promisor_remote_names`] order. A
/// source that cannot provide an object is skipped so the next promisor can be
/// tried; non-local transports remain the caller's responsibility.
pub fn hydrate_objects_from_local_promisor_remotes(
    git_dir: &Path,
    format: ObjectFormat,
    objects: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format)
        .with_promisor_remote_present(repo_has_promisor_remote(git_dir));
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let relative_base = if git_dir.file_name().is_some_and(|name| name == ".git") {
        git_dir.parent().unwrap_or(git_dir)
    } else {
        git_dir
    };
    let mut missing = objects
        .iter()
        .copied()
        .filter_map(|oid| match db.contains(&oid) {
            Ok(false) => Some(Ok(oid)),
            Ok(true) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<Vec<_>>>()?;
    missing.sort_by_key(ObjectId::to_hex);
    missing.dedup();
    let requested = missing.clone();

    for remote_name in crate::configured_promisor_remote_names(&config) {
        let Some(url) = config
            .get("remote", Some(&remote_name), "url")
            .filter(|url| !url.is_empty())
        else {
            continue;
        };
        let Ok(crate::fetch::FetchSource::Local {
            git_dir: promisor_git_dir,
            ..
        }) = crate::fetch_source_for_url(url, relative_base)
        else {
            continue;
        };
        crate::promisor::trace_promisor_remote_contact(&remote_name);
        for oid in missing.iter().copied() {
            let _ = install_fetch_pack_via_local_upload_pack(
                git_dir,
                &promisor_git_dir,
                format,
                vec![oid],
                None,
                true,
                false,
                Some(sley_odb::PackObjectFilter::BlobNone),
                Some(Vec::new()),
                false,
                None,
            );
        }
        db.refresh_read_cache();
        missing.retain(|oid| !db.contains(oid).unwrap_or(false));
        if missing.is_empty() {
            break;
        }
    }

    Ok(requested
        .into_iter()
        .filter(|oid| db.contains(oid).unwrap_or(false))
        .collect())
}

/// Hydrate every missing object in `starts`' reachable closure from configured
/// local promisor remotes.
///
/// This is used by operations such as a full repack after `.promisor`
/// sidecars were deliberately removed: repository configuration still proves
/// the remote is a promisor even though no local sidecar can classify the gap.
pub fn hydrate_reachable_from_local_promisor_remotes(
    remote_git_dir: &Path,
    format: ObjectFormat,
    starts: &[ObjectId],
) -> Result<()> {
    let remote_db =
        FileObjectDatabase::from_git_dir(remote_git_dir, format).with_promisor_remote_present(true);
    hydrate_reachable_promised_objects(remote_git_dir, &remote_db, format, starts, &HashSet::new())
}

fn hydrate_reachable_promised_objects(
    remote_git_dir: &Path,
    remote_db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    excluded: &HashSet<ObjectId>,
) -> Result<()> {
    let config = sley_config::read_repo_config(remote_git_dir, None).unwrap_or_default();
    let relative_base = if remote_git_dir
        .file_name()
        .is_some_and(|name| name == ".git")
    {
        remote_git_dir.parent().unwrap_or(remote_git_dir)
    } else {
        remote_git_dir
    };

    loop {
        let reachable = sley_odb::collect_reachable_object_ids_tolerating_missing(
            remote_db,
            format,
            starts.iter().copied(),
        )?;
        let mut missing = reachable
            .into_iter()
            .filter(|oid| !excluded.contains(oid))
            .filter_map(|oid| match remote_db.contains(&oid) {
                Ok(false) => Some(Ok(oid)),
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            })
            .collect::<Result<Vec<_>>>()?;
        missing.sort_by_key(ObjectId::to_hex);
        if missing.is_empty() {
            return Ok(());
        }

        let before = missing.len();
        for remote_name in sley_config::remotes::remote_names(&config)
            .into_iter()
            .filter(|name| {
                config
                    .get_bool("remote", Some(name), "promisor")
                    .unwrap_or(false)
                    || config
                        .get("remote", Some(name), "partialCloneFilter")
                        .is_some_and(|value| !value.is_empty())
            })
        {
            let Some(url) = config
                .get("remote", Some(&remote_name), "url")
                .filter(|url| !url.is_empty())
            else {
                continue;
            };
            let Ok(crate::fetch::FetchSource::Local {
                git_dir: promisor_git_dir,
                ..
            }) = crate::fetch_source_for_url(url, relative_base)
            else {
                continue;
            };
            crate::promisor::trace_promisor_remote_contact(&remote_name);
            let remote_before = missing.len();
            for oid in missing.iter().copied() {
                let _ = install_fetch_pack_via_local_upload_pack(
                    remote_git_dir,
                    &promisor_git_dir,
                    format,
                    vec![oid],
                    None,
                    true,
                    false,
                    None,
                    Some(Vec::new()),
                    false,
                    None,
                );
            }
            remote_db.refresh_read_cache();
            missing.retain(|oid| !remote_db.contains(oid).unwrap_or(false));
            if missing.len() < remote_before
                && config.get("remote", Some(&remote_name), "partialCloneFilter")
                    != Some("blob:none")
            {
                crate::apply_promisor_remote_field_updates(
                    remote_git_dir,
                    &[crate::PromisorRemoteFieldUpdate {
                        remote_name: remote_name.clone(),
                        field: crate::PromisorRemoteField::PartialCloneFilter,
                        previous: config
                            .get("remote", Some(&remote_name), "partialCloneFilter")
                            .map(str::to_string),
                        value: "blob:none".into(),
                    }],
                )?;
            }
            if missing.is_empty() {
                break;
            }
        }
        if missing.len() == before {
            return Err(GitError::object_not_found(missing[0]));
        }
    }
}

/// Inputs for one in-process protocol-v2 fetch using `want-ref`.
pub(crate) struct LocalProtocolV2FetchRequest<'a> {
    pub git_dir: &'a Path,
    pub destination_git_dir: &'a Path,
    pub remote_git_dir: &'a Path,
    pub format: ObjectFormat,
    pub wants: Vec<ObjectId>,
    pub want_refs: Vec<String>,
    pub haves: Option<Vec<ObjectId>>,
    /// Maximum raw pack bytes (`fetch.maxInputSize` / `transfer.maxSize`).
    /// `None` means unlimited.
    pub max_input_size: Option<u64>,
}

/// Structured result of an in-process protocol-v2 `want-ref` fetch.
#[derive(Debug, Default)]
pub(crate) struct LocalProtocolV2FetchOutcome {
    pub wanted_refs: Vec<ProtocolV2FetchWantedRef>,
    pub shallow_info: Vec<ProtocolV2FetchShallowInfo>,
}

/// Round-trip a normal, non-shallow local fetch through protocol v2 so a
/// ref-in-want capable server resolves ref names at request time rather than
/// relying on the earlier `ls-refs` snapshot.
pub(crate) fn install_fetch_pack_via_local_protocol_v2(
    input: LocalProtocolV2FetchRequest<'_>,
    cancel: CancelFlag<'_>,
) -> Result<LocalProtocolV2FetchOutcome> {
    if input.wants.is_empty() && input.want_refs.is_empty() {
        return Ok(LocalProtocolV2FetchOutcome::default());
    }

    let config = sley_config::read_repo_config(input.remote_git_dir, None).unwrap_or_default();
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: upload_pack_v2_capabilities(input.format, &config)?,
    };
    let haves = input
        .haves
        .map(Ok)
        .unwrap_or_else(|| local_have_oids(input.git_dir, input.format))?;
    let local_db = FileObjectDatabase::from_git_dir(input.git_dir, input.format);
    let destination_db = FileObjectDatabase::from_git_dir(input.destination_git_dir, input.format);
    let remote_db = FileObjectDatabase::from_git_dir(input.remote_git_dir, input.format);
    let negotiation_rounds = protocol_v2_negotiation_rounds(&local_db, &remote_db, &haves)?;
    let fetch = ProtocolV2FetchRequest {
        wants: input.wants,
        want_refs: input.want_refs,
        haves,
        thin_pack: true,
        include_tag: true,
        ofs_delta: true,
        done: true,
        wait_for_done: true,
        ..ProtocolV2FetchRequest::default()
    };

    // Exercise the same request codec and feature validation as a network
    // transport. Besides guarding the in-process boundary, the write emits the
    // exact `want`/`want-ref` packet trace expected from a v2 client.
    let mut request_bytes = Vec::new();
    write_protocol_v2_fetch_request(&mut request_bytes, &fetch)?;
    let command = read_protocol_v2_command_request(&mut request_bytes.as_slice())?;
    let decoded = match classify_protocol_v2_command_request(&handshake, input.format, &command)? {
        sley_protocol::ProtocolV2Command::Fetch(fetch) => fetch,
        _ => {
            return Err(GitError::InvalidFormat(
                "local protocol-v2 fetch decoded as a non-fetch command".into(),
            ));
        }
    };

    let mut sections = local_fetch_v2_sections(input.remote_git_dir, input.format, &decoded)?;
    // If every resolved want is already local, the pack builder has nothing to
    // send. Keep the wanted-refs mapping but omit an empty packfile section.
    sections.retain(|section| {
        !matches!(section, ProtocolV2FetchResponseSection::Packfile(lines) if lines.is_empty())
    });
    let mut response_bytes = Vec::new();
    write_protocol_v2_fetch_response(&mut response_bytes, &sections)?;

    let (header, _) = install_protocol_v2_fetch_response_from_reader_with_cancel(
        input.format,
        &mut response_bytes.as_slice(),
        false,
        &destination_db,
        input.max_input_size,
        cancel,
    )?;
    let mut outcome = LocalProtocolV2FetchOutcome::default();
    for section in header.sections {
        match section {
            ProtocolV2FetchResponseSection::WantedRefs(wanted) => {
                outcome.wanted_refs.extend(wanted);
            }
            ProtocolV2FetchResponseSection::ShallowInfo(shallow) => {
                outcome.shallow_info.extend(shallow);
            }
            _ => {}
        }
    }
    sley_core::trace2::data("negotiation_v2", "total_rounds", negotiation_rounds);
    Ok(outcome)
}

fn protocol_v2_negotiation_rounds(
    local_db: &FileObjectDatabase,
    remote_db: &FileObjectDatabase,
    haves: &[ObjectId],
) -> Result<usize> {
    let mut missing_commits = 0usize;
    for oid in haves {
        if !remote_db.contains(oid)? && local_db.read_object(oid)?.object_type == ObjectType::Commit
        {
            missing_commits += 1;
        }
    }
    // Git's consecutive negotiator starts with sixteen commits and doubles
    // the next batch. Count the request/response rounds needed before a common
    // commit can be reached; the final batch is the round carrying `done`.
    let mut rounds = 1usize;
    let mut batch = 16usize;
    while missing_commits > batch {
        missing_commits -= batch;
        batch = batch.saturating_mul(2);
        rounds += 1;
    }
    Ok(rounds)
}

fn local_upload_pack_client_wants_v2(git_dir: &Path) -> bool {
    sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| config.get("protocol", None, "version").map(str::to_string))
        .as_deref()
        == Some("2")
}

fn local_upload_pack_uses_bitmaps(git_dir: &Path) -> bool {
    let configured = sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| config.get_bool("pack", None, "usebitmaps"));
    configured.unwrap_or_else(|| {
        fs::read_dir(git_dir.join("objects/pack"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "bitmap"))
    })
}

fn repo_has_promisor_remote(git_dir: &Path) -> bool {
    let Ok(config) = sley_config::read_repo_config(git_dir, None) else {
        return false;
    };
    if config
        .get("extensions", None, "partialclone")
        .is_some_and(|value| !value.is_empty())
    {
        return true;
    }
    config.sections.iter().any(|section| {
        section.name.eq_ignore_ascii_case("remote")
            && section
                .subsection
                .as_deref()
                .is_some_and(|name| config.get_bool("remote", Some(name), "promisor") == Some(true))
    })
}

fn trace_local_upload_pack_v2_capabilities(remote_git_dir: &Path, format: ObjectFormat) {
    sley_protocol::set_packet_trace_identity("fetch");
    let config = sley_config::read_repo_config(remote_git_dir, None).unwrap_or_default();
    sley_protocol::trace_packet_read_payload(b"version 2\n");
    sley_protocol::trace_packet_read_payload(
        format!("agent={UPSTREAM_GIT_COMPAT_VERSION}\n").as_bytes(),
    );
    sley_protocol::trace_packet_read_payload(b"ls-refs=unborn\n");
    let mut fetch = "fetch=shallow wait-for-done".to_string();
    if config
        .get_bool("uploadpack", None, "allowfilter")
        .unwrap_or(false)
    {
        fetch.push_str(" filter");
    }
    if config
        .get_bool("uploadpack", None, "allowrefinwant")
        .unwrap_or(false)
    {
        fetch.push_str(" ref-in-want");
    }
    fetch.push('\n');
    sley_protocol::trace_packet_read_payload(fetch.as_bytes());
    sley_protocol::trace_packet_read_payload(
        format!("object-format={}\n", format.name()).as_bytes(),
    );
    sley_protocol::trace_packet_read_payload(b"0000");
}

pub(crate) fn upload_pack_filter_protocol_spec(
    filter: &sley_odb::PackObjectFilter,
) -> Option<String> {
    match filter {
        sley_odb::PackObjectFilter::BlobNone => Some("blob:none".to_string()),
        sley_odb::PackObjectFilter::BlobLimit(limit) => Some(format!("blob:limit={limit}")),
        sley_odb::PackObjectFilter::TreeDepth(depth) => Some(format!("tree:{depth}")),
        sley_odb::PackObjectFilter::SparsePathSet(_) => None,
    }
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
    let mut file = fs::OpenOptions::new().append(true).open(promisor_path)?;
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
        // upload-pack's trace2 `fetch-info` records the filter choice name,
        // not the normalized wire specification or its parameter.
        Some(sley_odb::PackObjectFilter::BlobLimit(_)) => "\"blob:limit\"".to_string(),
        Some(sley_odb::PackObjectFilter::TreeDepth(_)) => "\"tree\"".to_string(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LsRefsUnbornConfig {
    Ignore,
    Allow,
    Advertise,
}

fn lsrefs_unborn_config(config: &GitConfig) -> LsRefsUnbornConfig {
    match config.get("lsrefs", None, "unborn") {
        Some("ignore") => LsRefsUnbornConfig::Ignore,
        Some("allow") => LsRefsUnbornConfig::Allow,
        Some("advertise") | None => LsRefsUnbornConfig::Advertise,
        Some(_) => LsRefsUnbornConfig::Advertise,
    }
}

fn upload_pack_blob_packfile_uri_configured(config: &GitConfig) -> bool {
    config
        .get_all("uploadpack", None, "blobpackfileuri")
        .into_iter()
        .any(|value| value.is_some_and(|value| !value.is_empty()))
}

/// The v2 capabilities advertised by the upload-pack server, in the order git
/// emits them: `agent`, `ls-refs[=unborn]`, `fetch=<features>`,
/// `server-option`, `object-format=<hash>`.
fn upload_pack_v2_capabilities(
    format: ObjectFormat,
    config: &GitConfig,
) -> Result<Vec<Capability>> {
    let mut capabilities = vec![
        Capability {
            name: "agent".into(),
            value: Some(format!("git/{UPSTREAM_GIT_COMPAT_VERSION}")),
        },
        encode_protocol_v2_ls_refs_capability(&ProtocolV2LsRefsFeatures {
            unborn: lsrefs_unborn_config(config) == LsRefsUnbornConfig::Advertise,
            unknown: Vec::new(),
        })?,
        encode_protocol_v2_fetch_capability(&ProtocolV2FetchFeatures {
            shallow: true,
            wait_for_done: true,
            filter: config
                .get_bool("uploadpack", None, "allowfilter")
                .unwrap_or(false),
            ref_in_want: config
                .get_bool("uploadpack", None, "allowrefinwant")
                .unwrap_or(false),
            packfile_uris: upload_pack_blob_packfile_uri_configured(config),
            ..ProtocolV2FetchFeatures::default()
        })?,
        Capability {
            name: "server-option".into(),
            value: None,
        },
        Capability {
            name: "object-format".into(),
            value: Some(format.name().into()),
        },
    ];
    if config
        .get_bool("transfer", None, "advertisesid")
        .unwrap_or(false)
    {
        capabilities.push(Capability {
            name: "session-id".into(),
            value: Some("sley".into()),
        });
    }
    if let Some(capability) = crate::promisor_remote_server_capability(config)? {
        capabilities.push(capability);
    }
    // Advertise the `bundle-uri` command when the server is configured to hand
    // out bundle URIs (upstream `bundle_uri_advertise` reads
    // `uploadpack.advertisebundleuris`). The client then issues `command=bundle-uri`
    // to learn the `bundle.*` list before negotiating the pack.
    if config
        .get_bool("uploadpack", None, "advertisebundleuris")
        .unwrap_or(false)
    {
        capabilities.push(Capability {
            name: "bundle-uri".into(),
            value: None,
        });
    }
    Ok(capabilities)
}

/// Resolve the symref target of `HEAD` (e.g. `refs/heads/main`) for the
/// `symrefs`/symref-target ls-refs attribute, following one level of symbolic
/// indirection. Returns `None` for a detached or missing `HEAD`. When a
/// namespace is active the on-disk namespaced HEAD is read and the target is
/// returned in its logical (stripped) form.
fn head_symref_target(store: &FileRefStore) -> Result<Option<String>> {
    let head_name = sley_core::expand_namespace("HEAD");
    match store.read_ref(&head_name)? {
        Some(RefTarget::Symbolic(name)) => {
            let logical = sley_core::strip_namespace(&name)
                .unwrap_or(name.as_str())
                .to_string();
            Ok(Some(logical))
        }
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
    config: &GitConfig,
) -> Result<Vec<ProtocolV2LsRefsRecord>> {
    let store = FileRefStore::new(git_dir, format);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let namespace = sley_core::get_git_namespace();
    let hidden = transfer_upload_hidden_ref_patterns(git_dir);
    let head_symref = head_symref_target(&store)?;

    // Build the (name -> oid, symref) list in git's advertisement order: HEAD
    // first (when present), then the sorted ref list from `for-each-ref`.
    // Names are always the logical (namespace-stripped) form clients expect.
    let mut entries: Vec<(String, ObjectId, Option<String>)> = Vec::new();
    let head_physical = if namespace.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{namespace}HEAD")
    };
    if let Some(target) = store.read_ref(&head_physical)? {
        let reference = Ref {
            name: head_physical.clone(),
            target,
        };
        if !sley_core::ref_is_hidden(Some("HEAD"), &head_physical, &hidden) {
            if let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? {
                entries.push(("HEAD".to_string(), oid, head_symref.clone()));
            } else if request.unborn && lsrefs_unborn_config(config) != LsRefsUnbornConfig::Ignore {
                // An unborn HEAD (points at a not-yet-created branch) is reported as
                // an `unborn` record carrying its symref-target.
                entries.push((
                    "HEAD".to_string(),
                    ObjectId::null(format),
                    head_symref.clone(),
                ));
            }
        }
    }
    for reference in store.list_refs()? {
        let physical = reference.name.clone();
        let logical = if namespace.is_empty() {
            Some(physical.as_str())
        } else {
            physical.strip_prefix(namespace.as_str())
        };
        let Some(logical) = logical else {
            continue;
        };
        // Namespaced HEAD is under `refs/namespaces/.../HEAD`; skip the duplicate.
        if logical == "HEAD" {
            continue;
        }
        if sley_core::ref_is_hidden(Some(logical), &physical, &hidden) {
            continue;
        }
        let Some((oid, symref)) = resolve_for_each_ref_target(&store, &reference)? else {
            continue;
        };
        let logical_symref = symref.map(|s| {
            sley_core::strip_namespace(&s)
                .unwrap_or(s.as_str())
                .to_string()
        });
        entries.push((logical.to_string(), oid, logical_symref));
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
        let ready = acks
            .iter()
            .any(|ack| matches!(ack, ProtocolV2FetchAcknowledgment::Ack(_)));
        if ready {
            acks.push(ProtocolV2FetchAcknowledgment::Ready);
        }
        sections.push(ProtocolV2FetchResponseSection::Acknowledgments(acks));
        // Without a common commit, stop after acknowledgments so the client can
        // send its next batch. `wait-for-done` also stops after `ready`: the
        // client explicitly asked the server not to start the pack until a
        // subsequent request carries `done`.
        if !ready || request.wait_for_done {
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
    // A capability-only fetch command (for example the upstream
    // `packfile-uris` validation probe) has no pack request to answer. Git
    // accepts the command and emits no response section after advertising its
    // capabilities; emitting an empty `packfile` section is observably
    // different and can also corrupt a surrounding TAP stream.
    if wants.is_empty() {
        return Ok(sections);
    }

    // Apply upload-pack's shallow request before constructing the pack. The
    // HTTP server reaches this same in-process path, so rejecting an empty
    // `deepen-since` selection and cutting the pack at the computed boundary
    // must happen here rather than in a transport-specific client.
    let deepen_plan = if request.deepen_since.is_some() || !request.deepen_not.is_empty() {
        let advertisements = local_fetch_advertisements(git_dir, format)?;
        let mut deepen_not = Vec::with_capacity(request.deepen_not.len());
        for name in &request.deepen_not {
            let advertisement = advertisements
                .iter()
                .find(|advertisement| {
                    advertisement.name == *name
                        || advertisement.name == format!("refs/tags/{name}")
                        || advertisement.name == format!("refs/heads/{name}")
                        || advertisement.name == format!("refs/{name}")
                })
                .ok_or_else(|| {
                    GitError::Command(format!("git upload-pack: deepen-not is not a ref: {name}"))
                })?;
            deepen_not.push(advertisement.oid);
        }
        let since = request
            .deepen_since
            .map(|value| {
                i64::try_from(value).map_err(|_| {
                    GitError::InvalidFormat(format!("invalid deepen-since timestamp {value}"))
                })
            })
            .transpose()?;
        Some(compute_local_deepen_by_rev_list(
            &db,
            format,
            &wants,
            request.shallow.clone(),
            since,
            &deepen_not,
        )?)
    } else if let Some(depth) = request.deepen {
        Some(compute_local_deepen(
            &db,
            format,
            &wants,
            request.shallow.clone(),
            depth,
            request.deepen_relative,
        )?)
    } else {
        None
    };

    // Shallow-info: when the served repository is itself shallow and the client
    // did not request any deepening, report the shallow boundary commits that are
    // reachable from the wants (upstream `send_shallow_info`, which does an
    // implicit infinite-depth deepen on any fetch from a shallow repository). The
    // client uses these `shallow` lines to detect a shallow source — in
    // particular `git clone --reject-shallow` dies when they are present. The
    // section must precede the packfile section per gitprotocol-v2.
    let request_is_deepening = request.deepen.is_some()
        || request.deepen_since.is_some()
        || !request.deepen_not.is_empty();
    if !request_is_deepening {
        let server_shallow = crate::shallow::read_shallow(git_dir, format)?;
        if !server_shallow.is_empty() {
            let reachable = collect_reachable_object_ids(&db, format, wants.clone())?;
            let shallow_lines: Vec<ProtocolV2FetchShallowInfo> = server_shallow
                .iter()
                .filter(|oid| reachable.contains(*oid))
                .map(|oid| ProtocolV2FetchShallowInfo::Shallow(*oid))
                .collect();
            if !shallow_lines.is_empty() {
                sections.push(ProtocolV2FetchResponseSection::ShallowInfo(shallow_lines));
            }
        }
    } else if let Some(plan) = deepen_plan.as_ref()
        && !plan.shallow_info.is_empty()
    {
        sections.push(ProtocolV2FetchResponseSection::ShallowInfo(
            plan.shallow_info.clone(),
        ));
    }

    // Packfile section: build the reachable pack excluding the client's haves.
    let mut known_haves: Vec<ObjectId> = Vec::new();
    for have in &request.haves {
        if db.contains(have)? {
            known_haves.push(*have);
        }
    }
    let mut excluded = collect_reachable_object_ids(&db, format, known_haves)?;
    if let Some(plan) = deepen_plan {
        wants.extend(plan.extra_wants);
        excluded.extend(plan.excluded);
    }
    // Honor a partial-clone `filter` (blob:none / blob:limit=<n> / tree:<depth>):
    // upstream upload-pack applies the filter to the objects it packs. Without
    // this, a `--filter=blob:limit=0` clone would receive every blob and the
    // resulting "partial" clone would not actually be partial.
    let filter = request
        .filter
        .as_deref()
        .and_then(crate::pack_filter_from_spec);
    let pack = if filter.is_some() {
        build_reachable_pack_filtered(&db, format, wants, &excluded, filter)?
    } else {
        build_reachable_pack(&db, format, wants, &excluded)?
    }
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
/// Respond to a `command=bundle-uri` request by writing every `bundle.*` config
/// variable as a `key=value` packet line, terminated by a flush. Mirrors
/// upstream `bundle_uri_command` / `config_to_packet_line`: the section name is
/// lowercased, the subsection keeps its case, and the variable name is lowercased
/// (git's config normalization), e.g. `bundle.everything.uri=<uri>`.
fn write_bundle_uri_command_response(
    config: &GitConfig,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    for section in &config.sections {
        if !section.name.eq_ignore_ascii_case("bundle") {
            continue;
        }
        for entry in &section.entries {
            let Some(value) = entry.value.as_deref() else {
                continue;
            };
            let key = match &section.subsection {
                Some(subsection) => {
                    format!("bundle.{subsection}.{}", entry.key.to_ascii_lowercase())
                }
                None => format!("bundle.{}", entry.key.to_ascii_lowercase()),
            };
            write_pkt_line_payload(writer, format!("{key}={value}").as_bytes())?;
        }
    }
    write_pkt_line_frame(writer, &PktLineFrame::Flush)?;
    Ok(())
}

pub fn serve_upload_pack_v2(
    git_dir: &Path,
    format: ObjectFormat,
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    serve_upload_pack_v2_with_config(git_dir, format, &config, reader, writer)
}

pub fn serve_upload_pack_v2_with_config(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    serve_upload_pack_v2_inner(git_dir, format, config, reader, writer, true)
}

/// Serve a protocol-v2 stateless RPC request without re-advertising
/// capabilities. Smart HTTP performs capability discovery in its preceding
/// `info/refs` GET, so each upload-pack POST begins directly with the command
/// response, matching `git upload-pack --stateless-rpc`.
pub fn serve_upload_pack_v2_stateless_with_config(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
) -> Result<()> {
    serve_upload_pack_v2_inner(git_dir, format, config, reader, writer, false)
}

fn serve_upload_pack_v2_inner(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    advertise: bool,
) -> Result<()> {
    let handshake = TransportHandshake {
        protocol: ProtocolVersion::V2,
        capabilities: upload_pack_v2_capabilities(format, config)?,
    };
    if advertise {
        write_protocol_v2_advertisement(writer, &handshake)?;
        writer.flush()?;
    }

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
        // `command=bundle-uri` is not part of the fetch/ls-refs classification;
        // handle it directly by streaming the repository's `bundle.*` config as
        // `key=value` packet lines (upstream `bundle_uri_command`).
        if request.command == "bundle-uri" {
            write_bundle_uri_command_response(config, writer)?;
            writer.flush()?;
            continue;
        }
        match classify_protocol_v2_command_request(&handshake, format, &request)? {
            sley_protocol::ProtocolV2Command::LsRefs(ls_refs) => {
                let records = local_ls_refs_v2_records(git_dir, format, &ls_refs, config)?;
                write_protocol_v2_ls_refs_response(writer, &records)?;
                writer.flush()?;
            }
            sley_protocol::ProtocolV2Command::Fetch(fetch) => {
                let sections = local_fetch_v2_sections(git_dir, format, &fetch)?;
                if !sections.is_empty() {
                    write_protocol_v2_fetch_response(writer, &sections)?;
                    writer.flush()?;
                }
            }
            sley_protocol::ProtocolV2Command::ObjectInfo(_)
            | sley_protocol::ProtocolV2Command::Unknown(_) => {
                return Err(GitError::InvalidFormat(format!(
                    "unsupported protocol v2 command {}",
                    request.command
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_object::{BString, EncodedObject, Tree, TreeEntry};
    use sley_odb::ObjectWriter;

    #[test]
    fn filtered_clone_and_exact_checkout_hydration_install_separate_promisor_packs() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let root = unique_local_test_dir("split-partial-clone");
            let remote_git = root.join("remote.git");
            let client_git = root.join("client.git");
            for git_dir in [&remote_git, &client_git] {
                fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
                fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                    .expect("test repository HEAD");
            }
            let remote_db = FileObjectDatabase::from_git_dir(&remote_git, format);
            let blob = write_test_object(
                &remote_db,
                &EncodedObject::new(ObjectType::Blob, b"checkout payload\n".to_vec()),
            );
            let tree = write_test_object(&remote_db, &test_tree(&[(0o100644, b"file", blob)]));
            let commit = write_test_object(&remote_db, &test_commit(tree, &[], b"tip\n"));

            install_fetch_pack_via_local_upload_pack_with_promisor_decision_into(
                &client_git,
                &client_git,
                &remote_git,
                format,
                vec![commit],
                None,
                true,
                false,
                Some(sley_odb::PackObjectFilter::BlobNone),
                None,
                false,
                Some(1),
                &crate::PromisorRemoteDecision::default(),
                LocalFetchPackRequestMode::Traversal,
            )
            .expect("install filtered refs pack");
            let client_db = FileObjectDatabase::from_git_dir(&client_git, format);
            assert!(client_db.contains(&commit).expect("read filtered commit"));
            assert!(client_db.contains(&tree).expect("read filtered tree"));
            assert!(!client_db.contains(&blob).expect("check filtered blob"));

            install_fetch_pack_via_local_upload_pack(
                &client_git,
                &remote_git,
                format,
                vec![blob],
                None,
                true,
                false,
                None,
                None,
                false,
                Some(1),
            )
            .expect("install exact checkout blob pack");
            assert!(client_db.contains(&blob).expect("read hydrated blob"));
            let promisor_packs = fs::read_dir(client_git.join("objects/pack"))
                .expect("read client packs")
                .filter_map(std::result::Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "promisor")
                })
                .count();
            assert_eq!(promisor_packs, 2);
            fs::remove_dir_all(root).expect("remove test repositories");
        }
    }

    #[test]
    fn multi_round_negotiation_resets_in_vain_on_unrelated_ack() {
        let git_dir = unique_local_test_dir("negotiation-in-vain-reset");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree_oid = write_test_object(&db, &test_tree(&[]));
        let side_common = write_test_object(&db, &test_commit(tree_oid, &[], b"side\n"));
        let main_common = write_test_object(&db, &test_commit(tree_oid, &[], b"main\n"));
        let want = write_test_object(&db, &test_commit(tree_oid, &[main_common], b"want\n"));

        let missing = |value: usize| {
            ObjectId::from_hex(format, &format!("{value:040x}")).expect("synthetic negotiation oid")
        };
        let mut haves = (1..=255).map(missing).collect::<Vec<_>>();
        haves.push(side_common);
        haves.extend((256..=510).map(missing));
        haves.push(main_common);

        let outcome = negotiate_local_upload_pack(&db, format, &[want], haves)
            .expect("multi-round negotiation");
        assert_eq!(outcome.total_rounds, 6);
        assert_eq!(outcome.common_haves, vec![side_common, main_common]);

        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn multi_round_negotiation_does_not_give_up_before_first_ack() {
        let git_dir = unique_local_test_dir("negotiation-first-ack");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree_oid = write_test_object(&db, &test_tree(&[]));
        let common = write_test_object(&db, &test_commit(tree_oid, &[], b"common\n"));
        let want = write_test_object(&db, &test_commit(tree_oid, &[common], b"want\n"));
        let mut haves = (1..=496)
            .map(|value| {
                ObjectId::from_hex(format, &format!("{value:040x}"))
                    .expect("synthetic negotiation oid")
            })
            .collect::<Vec<_>>();
        haves.push(common);

        let outcome = negotiate_local_upload_pack(&db, format, &[want], haves)
            .expect("multi-round negotiation");
        assert_eq!(outcome.total_rounds, 6);
        assert_eq!(outcome.common_haves, vec![common]);

        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn negotiation_haves_stop_behind_remote_advertised_commit_tips() {
        let root = unique_local_test_dir("negotiation-advertised-tip");
        let remote_git = root.join("remote.git");
        let client_git = root.join("client.git");
        for git_dir in [&remote_git, &client_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::create_dir_all(git_dir.join("refs/heads")).expect("test repository refs");
            fs::create_dir_all(git_dir.join("refs/tags")).expect("test repository tags");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }

        let format = ObjectFormat::Sha1;
        let client_db = FileObjectDatabase::from_git_dir(&client_git, format);
        let remote_db = FileObjectDatabase::from_git_dir(&remote_git, format);
        let tree = test_tree(&[]);
        let tree_oid = write_test_object(&client_db, &tree);
        write_test_object(&remote_db, &tree);
        let root_commit = test_commit(tree_oid, &[], b"root\n");
        let root_oid = write_test_object(&client_db, &root_commit);
        write_test_object(&remote_db, &root_commit);
        let common_commit = test_commit(tree_oid, &[root_oid], b"common\n");
        let common_oid = write_test_object(&client_db, &common_commit);
        write_test_object(&remote_db, &common_commit);
        let client_commit = test_commit(tree_oid, &[common_oid], b"client\n");
        let client_oid = write_test_object(&client_db, &client_commit);
        let remote_commit = test_commit(tree_oid, &[common_oid], b"remote\n");
        let remote_oid = write_test_object(&remote_db, &remote_commit);
        fs::write(
            client_git.join("refs/heads/main"),
            format!("{client_oid}\n"),
        )
        .expect("client main ref");
        fs::write(
            client_git.join("refs/tags/common"),
            format!("{common_oid}\n"),
        )
        .expect("client common tag");
        fs::write(
            remote_git.join("refs/heads/main"),
            format!("{remote_oid}\n"),
        )
        .expect("remote main ref");
        fs::write(
            remote_git.join("refs/tags/common"),
            format!("{common_oid}\n"),
        )
        .expect("remote common tag");

        let haves =
            local_negotiation_have_oids_pruned_by_remote_tips(&client_git, &remote_git, format)
                .expect("plan negotiation haves");
        assert!(haves.contains(&client_oid));
        assert!(haves.contains(&common_oid));
        assert!(!haves.contains(&root_oid));

        fs::remove_dir_all(root).expect("remove test repositories");
    }

    #[test]
    fn negotiation_ignores_advertised_tags_with_promised_missing_targets() {
        for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
            let root = unique_local_test_dir("negotiation-promised-tag-target");
            let source_git = root.join("source.git");
            let remote_git = root.join("remote.git");
            let client_git = root.join("client.git");
            for git_dir in [&source_git, &remote_git, &client_git] {
                fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
                fs::create_dir_all(git_dir.join("refs/heads")).expect("test repository heads");
                fs::create_dir_all(git_dir.join("refs/tags")).expect("test repository tags");
                fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                    .expect("test repository HEAD");
            }

            let source_db = FileObjectDatabase::from_git_dir(&source_git, format);
            let promised_blob = write_test_object(
                &source_db,
                &EncodedObject::new(ObjectType::Blob, b"promised tag target\n".to_vec()),
            );
            let tree = write_test_object(&source_db, &test_tree(&[]));
            let commit = write_test_object(&source_db, &test_commit(tree, &[], b"main\n"));
            let tag = write_test_object(
                &source_db,
                &EncodedObject::new(
                    ObjectType::Tag,
                    Tag {
                        object: promised_blob,
                        object_type: ObjectType::Blob,
                        name: b"promised-blob".to_vec(),
                        tagger: None,
                        message: b"promised blob tag\n".to_vec(),
                        raw_body: None,
                    }
                    .write(),
                ),
            );

            sley_odb::build_and_install_reachable_pack_filtered(
                &source_db,
                &FileObjectDatabase::from_git_dir(&remote_git, format),
                format,
                [commit, tag],
                &HashSet::new(),
                RawPackInstallOptions {
                    promisor: true,
                    ..Default::default()
                },
                Some(sley_odb::PackObjectFilter::BlobNone),
                None,
            )
            .expect("build partial remote")
            .expect("install partial remote pack");
            sley_odb::build_and_install_reachable_pack_filtered(
                &source_db,
                &FileObjectDatabase::from_git_dir(&client_git, format),
                format,
                [tag],
                &HashSet::new(),
                RawPackInstallOptions {
                    promisor: true,
                    ..Default::default()
                },
                Some(sley_odb::PackObjectFilter::BlobNone),
                None,
            )
            .expect("build partial client")
            .expect("install partial client pack");
            fs::write(
                remote_git.join("config"),
                b"[extensions]\n\tpartialClone = origin\n[remote \"origin\"]\n\tpromisor = true\n",
            )
            .expect("partial remote config");
            fs::write(
                client_git.join("config"),
                b"[extensions]\n\tpartialClone = origin\n[remote \"origin\"]\n\tpromisor = true\n",
            )
            .expect("partial client config");
            fs::write(remote_git.join("refs/heads/main"), format!("{commit}\n"))
                .expect("remote main ref");
            fs::write(
                remote_git.join("refs/tags/promised-blob"),
                format!("{tag}\n"),
            )
            .expect("remote tag ref");
            fs::write(
                client_git.join("refs/tags/promised-blob"),
                format!("{tag}\n"),
            )
            .expect("client tag ref");

            let remote_db = FileObjectDatabase::from_git_dir(&remote_git, format)
                .with_promisor_remote_present(true);
            assert!(remote_db.contains(&commit).expect("remote commit lookup"));
            assert!(remote_db.contains(&tag).expect("remote tag lookup"));
            assert!(
                !remote_db
                    .contains(&promised_blob)
                    .expect("remote blob lookup")
            );
            assert!(remote_db.is_promised_object(&promised_blob));
            let client_db = FileObjectDatabase::from_git_dir(&client_git, format)
                .with_promisor_remote_present(true);
            assert!(client_db.contains(&tag).expect("client tag lookup"));
            assert!(
                !client_db
                    .contains(&promised_blob)
                    .expect("client blob lookup")
            );
            assert!(client_db.is_promised_object(&promised_blob));

            install_fetch_pack_via_local_upload_pack(
                &client_git,
                &remote_git,
                format,
                vec![commit],
                None,
                false,
                false,
                None,
                None,
                false,
                None,
            )
            .expect("unrelated branch fetch ignores promised tag target");
            assert!(
                FileObjectDatabase::from_git_dir(&client_git, format)
                    .contains(&commit)
                    .expect("client commit lookup")
            );

            fs::remove_dir_all(root).expect("remove test repositories");
        }
    }

    #[test]
    fn receive_pack_advertises_no_thin_until_server_fixes_thin_packs() {
        let features = receive_pack_features(ObjectFormat::Sha1);
        assert!(features.no_thin);

        let capabilities =
            encode_receive_pack_features(&features).expect("test operation should succeed");
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.name == "no-thin")
        );
    }

    #[test]
    fn protocol_v2_local_fetch_resolves_want_ref_and_installs_tip() {
        let root = unique_local_test_dir("protocol-v2-want-ref");
        let remote_git = root.join("remote.git");
        let client_git = root.join("client.git");
        for git_dir in [&remote_git, &client_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }
        fs::create_dir_all(remote_git.join("refs/heads")).expect("test repository refs");
        fs::write(
            remote_git.join("config"),
            b"[uploadpack]\n\tallowRefInWant = true\n",
        )
        .expect("test repository config");

        let format = ObjectFormat::Sha1;
        let remote_db = FileObjectDatabase::from_git_dir(&remote_git, format);
        let blob_oid = write_test_object(
            &remote_db,
            &EncodedObject::new(ObjectType::Blob, b"wanted\n".to_vec()),
        );
        let tree_oid = write_test_object(&remote_db, &test_tree(&[(0o100644, b"file", blob_oid)]));
        let commit_oid = write_test_object(&remote_db, &test_commit(tree_oid, &[], b"tip\n"));
        fs::write(
            remote_git.join("refs/heads/main"),
            format!("{commit_oid}\n"),
        )
        .expect("test repository main ref");

        let outcome = install_fetch_pack_via_local_protocol_v2(
            LocalProtocolV2FetchRequest {
                git_dir: &client_git,
                destination_git_dir: &client_git,
                remote_git_dir: &remote_git,
                format,
                wants: Vec::new(),
                want_refs: vec!["refs/heads/main".into()],
                haves: Some(Vec::new()),
                max_input_size: None,
            },
            CancelFlag::never(),
        )
        .expect("protocol-v2 want-ref fetch");

        assert_eq!(outcome.wanted_refs.len(), 1);
        assert_eq!(outcome.wanted_refs[0].name, "refs/heads/main");
        assert_eq!(outcome.wanted_refs[0].oid, commit_oid);
        assert!(
            FileObjectDatabase::from_git_dir(&client_git, format)
                .contains(&commit_oid)
                .expect("client object lookup")
        );
        fs::remove_dir_all(root).expect("remove test repositories");
    }

    #[test]
    fn protocol_v2_upload_pack_rejects_deepen_since_after_every_commit() {
        let git_dir = unique_local_test_dir("protocol-v2-empty-deepen-since");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");

        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree_oid = write_test_object(&db, &test_tree(&[]));
        let commit_oid = write_test_object(&db, &test_commit(tree_oid, &[], b"tip\n"));
        let error = local_fetch_v2_sections(
            &git_dir,
            format,
            &ProtocolV2FetchRequest {
                wants: vec![commit_oid],
                deepen_since: Some(1),
                done: true,
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect_err("a cutoff after every commit must fail");

        assert!(
            error
                .to_string()
                .contains("no commits selected for shallow requests")
        );
        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn protocol_v2_upload_pack_negotiates_ready_before_pack() {
        let git_dir = unique_local_test_dir("protocol-v2-negotiation");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");

        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree_oid = write_test_object(&db, &test_tree(&[]));
        let base = write_test_object(&db, &test_commit(tree_oid, &[], b"base\n"));
        let tip = write_test_object(&db, &test_commit(tree_oid, &[base], b"tip\n"));
        let sections = local_fetch_v2_sections(
            &git_dir,
            format,
            &ProtocolV2FetchRequest {
                wants: vec![tip],
                haves: vec![base],
                done: false,
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect("common have makes the server ready");
        assert_eq!(
            sections.first(),
            Some(&ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Ack(base),
                ProtocolV2FetchAcknowledgment::Ready,
            ]))
        );
        assert!(matches!(
            sections.last(),
            Some(ProtocolV2FetchResponseSection::Packfile(_))
        ));

        let unknown = ObjectId::from_hex(format, "1111111111111111111111111111111111111111")
            .expect("test object id");
        let sections = local_fetch_v2_sections(
            &git_dir,
            format,
            &ProtocolV2FetchRequest {
                wants: vec![tip],
                haves: vec![unknown],
                done: false,
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect("unknown have continues negotiation");
        assert_eq!(
            sections,
            vec![ProtocolV2FetchResponseSection::Acknowledgments(vec![
                ProtocolV2FetchAcknowledgment::Nak,
            ])]
        );

        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn protocol_v2_fetch_without_wants_emits_no_empty_packfile_section() {
        let git_dir = unique_local_test_dir("protocol-v2-no-wants");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");

        let sections = local_fetch_v2_sections(
            &git_dir,
            ObjectFormat::Sha1,
            &ProtocolV2FetchRequest {
                done: true,
                packfile_uris: Some("https".into()),
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect("capability-only fetch request");
        assert!(sections.is_empty());

        let config = GitConfig::parse(b"[uploadpack]\n\tblobpackfileuri = anything\n")
            .expect("packfile-uri config");
        let request = ProtocolV2FetchRequest {
            done: true,
            packfile_uris: Some("https".into()),
            ..ProtocolV2FetchRequest::default()
        };
        let mut request_bytes = Vec::new();
        write_protocol_v2_fetch_request(&mut request_bytes, &request)
            .expect("encode capability-only request");

        let mut stateless_response = Vec::new();
        serve_upload_pack_v2_stateless_with_config(
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            &mut request_bytes.as_slice(),
            &mut stateless_response,
        )
        .expect("serve stateless capability-only request");
        assert!(stateless_response.is_empty());

        let mut long_lived_response = Vec::new();
        serve_upload_pack_v2_with_config(
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            &mut request_bytes.as_slice(),
            &mut long_lived_response,
        )
        .expect("serve long-lived capability-only request");
        let handshake = TransportHandshake {
            protocol: ProtocolVersion::V2,
            capabilities: upload_pack_v2_capabilities(ObjectFormat::Sha1, &config)
                .expect("test capabilities"),
        };
        let mut advertisement = Vec::new();
        write_protocol_v2_advertisement(&mut advertisement, &handshake)
            .expect("encode test advertisement");
        assert_eq!(long_lived_response, advertisement);
        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn protocol_v2_stateless_upload_pack_does_not_readvertise() {
        let git_dir = unique_local_test_dir("protocol-v2-stateless");
        fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
        fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("test repository HEAD");

        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(&git_dir, format);
        let tree_oid = write_test_object(&db, &test_tree(&[]));
        let tip = write_test_object(&db, &test_commit(tree_oid, &[], b"tip\n"));
        let mut request = Vec::new();
        write_protocol_v2_fetch_request(
            &mut request,
            &ProtocolV2FetchRequest {
                wants: vec![tip],
                done: true,
                ..ProtocolV2FetchRequest::default()
            },
        )
        .expect("encode fetch request");
        let mut response = Vec::new();
        serve_upload_pack_v2_stateless_with_config(
            &git_dir,
            format,
            &GitConfig::default(),
            &mut request.as_slice(),
            &mut response,
        )
        .expect("serve stateless fetch");

        assert_eq!(
            sley_protocol::read_pkt_line_frame(&mut response.as_slice())
                .expect("read response")
                .expect("response frame"),
            PktLineFrame::Data(b"packfile\n".to_vec())
        );
        fs::remove_dir_all(git_dir).expect("remove test repository");
    }

    #[test]
    fn local_fetch_from_incomplete_remote_excludes_client_have_closure() {
        let root = unique_local_test_dir("incomplete-local-fetch");
        let base_git = root.join("base.git");
        let patch_git = root.join("patch.git");
        let user_git = root.join("user.git");
        let direct_git = root.join("direct.git");
        for git_dir in [&base_git, &patch_git, &user_git, &direct_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test operation should succeed");
        }

        let format = ObjectFormat::Sha1;
        let base_db = FileObjectDatabase::from_git_dir(&base_git, format);
        let patch_db = FileObjectDatabase::from_git_dir(&patch_git, format);

        let text_a = EncodedObject::new(ObjectType::Blob, b"a\nb\nc\nd\ne\nf\ng\nh\ni\n".to_vec());
        let text_a_oid = write_test_object(&base_db, &text_a);
        let side = EncodedObject::new(ObjectType::Blob, b"side\n".to_vec());
        let side_oid = write_test_object(&base_db, &side);
        let tree_a = test_tree(&[
            (0o100644, b"side", side_oid),
            (0o100644, b"text", text_a_oid),
        ]);
        let tree_a_oid = write_test_object(&base_db, &tree_a);
        let commit_a = test_commit(tree_a_oid, &[], b"A\n");
        let commit_a_oid = write_test_object(&base_db, &commit_a);

        let text_b =
            EncodedObject::new(ObjectType::Blob, b"a\nb\nc\nd\ne\nf\ng\nh\ni\nm\n".to_vec());
        let text_b_oid = write_test_object(&base_db, &text_b);
        let tree_b = test_tree(&[
            (0o100644, b"side", side_oid),
            (0o100644, b"text", text_b_oid),
        ]);
        let tree_b_oid = write_test_object(&base_db, &tree_b);
        let commit_b = test_commit(tree_b_oid, &[commit_a_oid], b"B\n");
        let commit_b_oid = write_test_object(&base_db, &commit_b);

        let text_c = EncodedObject::new(
            ObjectType::Blob,
            b"a\nb\nc\nd\ne\nf\ng\nh\ni\nm\nq\n".to_vec(),
        );
        let text_c_oid = write_test_object(&patch_db, &text_c);
        let tree_c = test_tree(&[
            (0o100644, b"side", side_oid),
            (0o100644, b"text", text_c_oid),
        ]);
        let tree_c_oid = write_test_object(&patch_db, &tree_c);
        let commit_c = test_commit(tree_c_oid, &[commit_b_oid], b"C\n");
        let commit_c_oid = write_test_object(&patch_db, &commit_c);
        write_test_object(&patch_db, &tree_b);
        write_test_object(&patch_db, &commit_b);
        assert!(
            !patch_db
                .contains(&text_b_oid)
                .expect("test operation should succeed"),
            "patch repo must be missing the best delta base"
        );

        install_fetch_pack_via_local_upload_pack(
            &user_git,
            &base_git,
            format,
            vec![commit_b_oid],
            None,
            false,
            false,
            None,
            None,
            false,
            None,
        )
        .expect("base fetch should succeed");
        assert!(
            FileObjectDatabase::from_git_dir(&user_git, format)
                .contains(&text_b_oid)
                .expect("test operation should succeed"),
            "user clone should have the missing base before fetching C"
        );

        install_fetch_pack_via_local_upload_pack(
            &user_git,
            &patch_git,
            format,
            vec![commit_c_oid],
            None,
            false,
            false,
            None,
            None,
            false,
            None,
        )
        .expect("fetch from incomplete remote should succeed when client has the base");
        assert!(
            FileObjectDatabase::from_git_dir(&user_git, format)
                .contains(&commit_c_oid)
                .expect("test operation should succeed")
        );

        let direct = install_fetch_pack_via_local_upload_pack(
            &direct_git,
            &patch_git,
            format,
            vec![commit_c_oid],
            None,
            false,
            false,
            None,
            None,
            false,
            None,
        );
        assert!(
            direct.is_err(),
            "direct fetch from the incomplete patch repo must still fail"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn direct_promisor_object_fetch_ignores_blob_none_for_explicit_blob() {
        let root = unique_local_test_dir("direct-promisor-object-fetch");
        let remote_git = root.join("remote.git");
        let client_git = root.join("client.git");
        for git_dir in [&remote_git, &client_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test operation should succeed");
        }

        let format = ObjectFormat::Sha1;
        let remote_db = FileObjectDatabase::from_git_dir(&remote_git, format);
        let client_db = FileObjectDatabase::from_git_dir(&client_git, format);

        let blob = EncodedObject::new(ObjectType::Blob, b"promised\n".to_vec());
        let blob_oid = write_test_object(&remote_db, &blob);
        let tree = test_tree(&[(0o100644, b"file.txt", blob_oid)]);
        let tree_oid = write_test_object(&remote_db, &tree);
        let commit = test_commit(tree_oid, &[], b"main\n");
        let commit_oid = write_test_object(&remote_db, &commit);

        write_test_object(&client_db, &tree);
        write_test_object(&client_db, &commit);
        assert!(
            !client_db
                .contains(&blob_oid)
                .expect("test operation should succeed"),
            "client starts with the promised blob missing"
        );

        install_fetch_pack_via_local_upload_pack(
            &client_git,
            &remote_git,
            format,
            vec![blob_oid],
            None,
            true,
            false,
            Some(sley_odb::PackObjectFilter::BlobNone),
            None,
            false,
            None,
        )
        .expect("direct promisor blob fetch must ignore blob:none for the explicit want");

        assert!(
            FileObjectDatabase::from_git_dir(&client_git, format)
                .contains(&blob_oid)
                .expect("test operation should succeed"),
            "directly wanted promised blob should be installed"
        );
        assert_eq!(
            FileObjectDatabase::from_git_dir(&client_git, format)
                .read_object(&commit_oid)
                .expect("test operation should succeed")
                .object_type,
            ObjectType::Commit
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn exact_object_hydration_uses_configured_local_promisors() {
        let root = unique_local_test_dir("exact-promisor-hydration");
        let client_git = root.join("client.git");
        let origin_git = root.join("origin.git");
        let lop_git = root.join("lop.git");
        for git_dir in [&client_git, &origin_git, &lop_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::create_dir_all(git_dir.join("refs")).expect("test repository refs");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }
        fs::write(
            client_git.join("config"),
            format!(
                "[extensions]\n\tpartialClone = origin\n\
                 [remote \"origin\"]\n\tpromisor = true\n\turl = {}\n\
                 [remote \"lop\"]\n\tpromisor = true\n\turl = {}\n",
                origin_git.display(),
                lop_git.display()
            ),
        )
        .expect("client promisor config");

        let format = ObjectFormat::Sha1;
        let blob = EncodedObject::new(ObjectType::Blob, b"lazy payload\n".to_vec());
        let blob_oid =
            write_test_object(&FileObjectDatabase::from_git_dir(&lop_git, format), &blob);
        let hydrated =
            hydrate_objects_from_local_promisor_remotes(&client_git, format, &[blob_oid])
                .expect("hydrate exact object");
        assert_eq!(hydrated, vec![blob_oid]);
        assert!(
            FileObjectDatabase::from_git_dir(&client_git, format)
                .contains(&blob_oid)
                .expect("client object lookup")
        );

        fs::remove_dir_all(root).expect("remove test repositories");
    }

    #[test]
    fn accepted_promisor_omits_gap_while_rejection_hydrates_server() {
        let root = unique_local_test_dir("promisor-traversal-policy");
        let origin_git = root.join("origin.git");
        let server_git = root.join("server.git");
        let lop_git = root.join("lop.git");
        let accepted_git = root.join("accepted.git");
        let rejected_git = root.join("rejected.git");
        for git_dir in [
            &origin_git,
            &server_git,
            &lop_git,
            &accepted_git,
            &rejected_git,
        ] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::create_dir_all(git_dir.join("refs")).expect("test repository refs");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }

        let format = ObjectFormat::Sha1;
        let origin_db = FileObjectDatabase::from_git_dir(&origin_git, format);
        let blob = EncodedObject::new(ObjectType::Blob, b"promised payload\n".to_vec());
        let blob_oid = write_test_object(&origin_db, &blob);
        let tree_oid =
            write_test_object(&origin_db, &test_tree(&[(0o100644, b"payload", blob_oid)]));
        let commit_oid = write_test_object(&origin_db, &test_commit(tree_oid, &[], b"tip\n"));
        write_test_object(&FileObjectDatabase::from_git_dir(&lop_git, format), &blob);

        sley_odb::build_and_install_reachable_pack_filtered(
            &origin_db,
            &FileObjectDatabase::from_git_dir(&server_git, format),
            format,
            vec![commit_oid],
            &HashSet::new(),
            RawPackInstallOptions {
                promisor: true,
                ..Default::default()
            },
            Some(sley_odb::PackObjectFilter::BlobNone),
            None,
        )
        .expect("create incomplete promisor server")
        .expect("promisor pack");
        fs::write(
            server_git.join("config"),
            format!(
                "[remote \"lop\"]\n\tpromisor = true\n\turl = {}\n",
                lop_git.display()
            ),
        )
        .expect("server promisor config");

        let advertisement = sley_protocol::PromisorRemoteAdvertisement {
            name: "lop".into(),
            url: lop_git.to_string_lossy().into_owned(),
            partial_clone_filter: None,
            token: None,
        };
        install_fetch_pack_via_local_upload_pack_with_promisor_decision(
            &accepted_git,
            &server_git,
            format,
            vec![commit_oid],
            None,
            true,
            false,
            Some(sley_odb::PackObjectFilter::BlobNone),
            Some(Vec::new()),
            false,
            None,
            &crate::PromisorRemoteDecision {
                accepted: vec![advertisement],
                reply: Some("lop".into()),
                stored_fields: Vec::new(),
            },
        )
        .expect("accepted promisor transfer");
        let server_db = FileObjectDatabase::from_git_dir(&server_git, format);
        assert!(!server_db.contains(&blob_oid).expect("server object lookup"));

        install_fetch_pack_via_local_upload_pack(
            &rejected_git,
            &server_git,
            format,
            vec![commit_oid],
            None,
            true,
            false,
            Some(sley_odb::PackObjectFilter::BlobNone),
            Some(Vec::new()),
            false,
            None,
        )
        .expect("rejected promisor transfer hydrates server");
        server_db.refresh_read_cache();
        assert!(
            server_db
                .contains(&blob_oid)
                .expect("hydrated server lookup")
        );
        fs::remove_dir_all(root).expect("remove test repositories");
    }

    #[test]
    fn configured_local_promisor_hydrates_reachable_gap_without_sidecar() {
        let root = unique_local_test_dir("promisor-repack-hydration");
        let origin_git = root.join("origin.git");
        let server_git = root.join("server.git");
        let lop_git = root.join("lop.git");
        for git_dir in [&origin_git, &server_git, &lop_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::create_dir_all(git_dir.join("refs")).expect("test repository refs");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }

        let format = ObjectFormat::Sha1;
        let origin_db = FileObjectDatabase::from_git_dir(&origin_git, format);
        let blob = EncodedObject::new(ObjectType::Blob, b"repack promised payload\n".to_vec());
        let blob_oid = write_test_object(&origin_db, &blob);
        let tree_oid =
            write_test_object(&origin_db, &test_tree(&[(0o100644, b"payload", blob_oid)]));
        let commit_oid = write_test_object(&origin_db, &test_commit(tree_oid, &[], b"tip\n"));
        write_test_object(&FileObjectDatabase::from_git_dir(&lop_git, format), &blob);

        sley_odb::build_and_install_reachable_pack_filtered(
            &origin_db,
            &FileObjectDatabase::from_git_dir(&server_git, format),
            format,
            vec![commit_oid],
            &HashSet::new(),
            RawPackInstallOptions {
                promisor: true,
                ..Default::default()
            },
            Some(sley_odb::PackObjectFilter::BlobNone),
            None,
        )
        .expect("create incomplete promisor server")
        .expect("promisor pack");
        for entry in fs::read_dir(server_git.join("objects/pack")).expect("pack directory") {
            let path = entry.expect("pack entry").path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("promisor") {
                fs::remove_file(path).expect("remove promisor classification");
            }
        }
        fs::write(
            server_git.join("config"),
            format!(
                "[remote \"lop\"]\n\tpromisor = true\n\turl = {}\n",
                lop_git.display()
            ),
        )
        .expect("server promisor config");

        let server_db = FileObjectDatabase::from_git_dir(&server_git, format);
        assert!(
            !server_db
                .contains(&blob_oid)
                .expect("missing before hydrate")
        );
        hydrate_reachable_from_local_promisor_remotes(&server_git, format, &[commit_oid])
            .expect("hydrate configured promisor gap");
        server_db.refresh_read_cache();
        assert!(server_db.contains(&blob_oid).expect("hydrated blob lookup"));

        fs::remove_dir_all(root).expect("remove test repositories");
    }

    #[test]
    fn promisor_hydration_skips_objects_excluded_by_client_haves() {
        let root = unique_local_test_dir("promisor-fetch-exclusions");
        let origin_git = root.join("origin.git");
        let server_git = root.join("server.git");
        let lop_git = root.join("lop.git");
        for git_dir in [&origin_git, &server_git, &lop_git] {
            fs::create_dir_all(git_dir.join("objects")).expect("test repository objects");
            fs::create_dir_all(git_dir.join("refs")).expect("test repository refs");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
                .expect("test repository HEAD");
        }

        let format = ObjectFormat::Sha1;
        let origin_db = FileObjectDatabase::from_git_dir(&origin_git, format);
        let old_blob = EncodedObject::new(ObjectType::Blob, b"old promised payload\n".to_vec());
        let new_blob = EncodedObject::new(ObjectType::Blob, b"new promised payload\n".to_vec());
        let old_blob_oid = write_test_object(&origin_db, &old_blob);
        let new_blob_oid = write_test_object(&origin_db, &new_blob);
        let tree_oid = write_test_object(
            &origin_db,
            &test_tree(&[
                (0o100644, b"old", old_blob_oid),
                (0o100644, b"new", new_blob_oid),
            ]),
        );
        let commit_oid = write_test_object(&origin_db, &test_commit(tree_oid, &[], b"tip\n"));
        let lop_db = FileObjectDatabase::from_git_dir(&lop_git, format);
        write_test_object(&lop_db, &old_blob);
        write_test_object(&lop_db, &new_blob);

        sley_odb::build_and_install_reachable_pack_filtered(
            &origin_db,
            &FileObjectDatabase::from_git_dir(&server_git, format),
            format,
            vec![commit_oid],
            &HashSet::new(),
            RawPackInstallOptions {
                promisor: true,
                ..Default::default()
            },
            Some(sley_odb::PackObjectFilter::BlobNone),
            None,
        )
        .expect("create incomplete promisor server")
        .expect("promisor pack");
        fs::write(
            server_git.join("config"),
            format!(
                "[remote \"lop\"]\n\tpromisor = true\n\turl = {}\n",
                lop_git.display()
            ),
        )
        .expect("server promisor config");

        let server_db = FileObjectDatabase::from_git_dir(&server_git, format)
            .with_promisor_remote_present(true);
        hydrate_reachable_promised_objects(
            &server_git,
            &server_db,
            format,
            &[commit_oid],
            &HashSet::from([old_blob_oid]),
        )
        .expect("hydrate only non-excluded gap");
        server_db.refresh_read_cache();
        assert!(!server_db.contains(&old_blob_oid).expect("old blob lookup"));
        assert!(server_db.contains(&new_blob_oid).expect("new blob lookup"));

        fs::remove_dir_all(root).expect("remove test repositories");
    }

    fn unique_local_test_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test operation should succeed")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("sley-remote-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).expect("test operation should succeed");
        root
    }

    fn write_test_object(db: &FileObjectDatabase, object: &EncodedObject) -> ObjectId {
        db.write_object(object.clone())
            .expect("test operation should succeed")
    }

    fn test_tree(entries: &[(u32, &[u8], ObjectId)]) -> EncodedObject {
        EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: entries
                    .iter()
                    .map(|(mode, name, oid)| TreeEntry {
                        mode: *mode,
                        name: BString::from(*name),
                        oid: *oid,
                    })
                    .collect(),
            }
            .write(),
        )
    }

    fn test_commit(tree: ObjectId, parents: &[ObjectId], message: &[u8]) -> EncodedObject {
        let identity = b"Example <example@example.invalid> 0 +0000".to_vec();
        EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree,
                parents: parents.to_vec(),
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: message.to_vec(),
            }
            .write(),
        )
    }
}

pub fn negotiate_only_local(
    local_git_dir: &Path,
    remote_git_dir: &Path,
    format: ObjectFormat,
    tip_oids: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let local_db = FileObjectDatabase::from_git_dir(local_git_dir, format);
    let mut seen = HashSet::new();
    let mut haves = Vec::new();
    // Walk each tip's commit ancestry so a tip that only exists locally still
    // reveals common ancestors the remote can ACK (fetch-pack negotiator).
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
            if object.object_type != ObjectType::Commit {
                continue;
            }
            if let Ok(commit) = Commit::parse_ref(format, &object.body) {
                for parent in commit.parents {
                    if !seen.contains(&parent) {
                        stack.push(parent);
                    }
                }
            }
        }
    }
    let request = ProtocolV2FetchRequest {
        haves,
        wait_for_done: true,
        done: false,
        thin_pack: true,
        ofs_delta: true,
        ..ProtocolV2FetchRequest::default()
    };
    let sections = local_fetch_v2_sections(remote_git_dir, format, &request)?;
    let mut acked = Vec::new();
    for section in sections {
        if let ProtocolV2FetchResponseSection::Acknowledgments(acks) = section {
            for ack in acks {
                if let ProtocolV2FetchAcknowledgment::Ack(oid) = ack {
                    acked.push(oid);
                }
            }
        }
    }
    Ok(acked)
}
