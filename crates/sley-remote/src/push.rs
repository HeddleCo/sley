//! Callable push orchestration for HTTP(S) and local (`file://`/path) remotes.
//!
//! [`push`] sequences the moved transport plumbing ([`crate::http`],
//! [`crate::local`]) and the protocol codecs ([`sley_protocol`]) into the full
//! push flow: it advertises the remote's refs, plans the receive-pack commands
//! for the requested refspecs, rejects non-fast-forward updates (unless forced),
//! builds the packfile of the objects the remote is missing, sends the
//! receive-pack request, and parses the report-status. Everything is taken as
//! explicit parameters — `git_dir`, `common_git_dir`, the [`ObjectFormat`], the
//! repository [`GitConfig`], the already-resolved destination, the push refspecs,
//! a [`PushOptions`], and the seam objects ([`CredentialProvider`],
//! [`ProgressSink`]) — so it never reads process-global state, parses arguments,
//! or prints. The structured result ([`PushOutcome`]) carries the executed
//! receive-pack commands and the remote's report-status for the caller to format
//! into git's "To <remote>" summary and to drive any set-upstream config write.
//!
//! SSH push still lives in the CLI; only HTTP and local move here. The
//! push-planning helpers are shared (the CLI's SSH path calls the same `pub`
//! functions) so there is a single implementation.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{
    build_reachable_pack, collect_reachable_object_ids, FileObjectDatabase, ObjectReader,
};
use sley_protocol::{
    build_receive_pack_push_request, parse_receive_pack_features, parse_refspec, plan_push_commands,
    read_receive_pack_report_status, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type, write_receive_pack_push_request, GitService, PushSourceRef,
    ReceivePackCommand, ReceivePackCommandStatus, ReceivePackFeatures, ReceivePackPushRequest,
    ReceivePackPushRequestOptions, ReceivePackReportStatus, ReceivePackRequest,
    ReceivePackUnpackStatus, RefAdvertisement,
};
use sley_refs::{FileRefStore, Ref, RefTarget};
use sley_transport::{http_smart_rpc_url, HttpClient, RemoteUrl};

use crate::{CredentialProvider, ProgressSink};

/// How a push delivers refs and objects to the remote.
///
/// The caller resolves the remote (URL rewriting, `pushurl` selection,
/// repository discovery — all process-state dependent) and hands `push` a
/// concrete transport.
pub enum PushDestination {
    /// A smart-HTTP(S) remote at the given already-resolved URL.
    Http(RemoteUrl),
    /// An SSH remote at the given already-resolved URL. Pushed by spawning `ssh`
    /// (the credential seam is unused — the `ssh` program owns authentication).
    Ssh(RemoteUrl),
    /// A local repository served in-process from `git_dir`.
    Local {
        /// The remote repository's `$GIT_DIR`.
        git_dir: PathBuf,
        /// The remote repository's common `$GIT_DIR` (object format source).
        common_git_dir: PathBuf,
    },
}

/// Controls for a [`push`] run, mirroring the `git push` flags the CLI parses
/// that affect the wire/planning behavior the library owns.
///
/// `set-upstream` (`-u`) is intentionally absent: it only writes
/// `branch.<name>.remote`/`merge` config, which is a caller concern (the library
/// returns the executed commands in [`PushOutcome::commands`] so the caller can
/// drive that write). Atomic / push-options / thin are likewise absent because
/// the CLI's HTTP and local push paths accept but do not act on them today; this
/// stays a faithful refactor of the existing behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct PushOptions {
    /// Suppress the per-command side-effect of negotiating the `quiet`
    /// receive-pack capability (matching `git push --quiet`). Output suppression
    /// itself is a caller concern — the library always returns the outcome.
    pub quiet: bool,
    /// Force every update, bypassing the non-fast-forward check. Per-refspec `+`
    /// forces are honored independently of this flag.
    pub force: bool,
}

/// The structured result of a [`push`].
#[derive(Debug, Clone, Default)]
pub struct PushOutcome {
    /// The receive-pack commands that were executed, in planning order. Each
    /// carries the ref name and its old/new object id; the caller formats these
    /// into git's "To <remote>" summary and uses them to drive set-upstream.
    /// Empty when nothing matched the refspecs (a no-op push).
    pub commands: Vec<ReceivePackCommand>,
    /// The remote's report-status, when one was requested and received (i.e. the
    /// remote advertised `report-status`). `None` when report-status was not
    /// negotiated. Already validated: a failed unpack or a rejected ref is
    /// surfaced as an `Err` from [`push`], not returned here.
    pub report: Option<ReceivePackReportStatus>,
}

/// Push `refspecs` to a resolved `destination` from the repository at `git_dir`.
///
/// Performs the work the CLI's `push_http_repository`/`push_local_repository`
/// did: advertises the remote's refs, plans the receive-pack commands for
/// `refspecs`, rejects non-fast-forward branch updates (unless forced), builds
/// the pack of objects the remote lacks, sends the receive-pack request, parses
/// and validates the report-status, and returns the executed commands. `remote`
/// is the remote/argument the caller resolved `destination` from (used only for
/// error messages here).
///
/// Returns the structured [`PushOutcome`]; never prints or returns
/// `GitError::Exit`. A still-`None` report in the outcome means the remote did
/// not advertise `report-status`. Set-upstream config and the "To <remote>"
/// summary are the caller's job, driven from [`PushOutcome::commands`].
#[allow(clippy::too_many_arguments)]
pub fn push(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    remote: &str,
    destination: &PushDestination,
    refspecs: &[String],
    options: &PushOptions,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
) -> Result<PushOutcome> {
    // `config` and `progress` are part of the seam (mirroring `fetch`) but the
    // current push flow drives credentials from the caller-built provider and
    // returns its summary in `PushOutcome` rather than streaming progress, so
    // neither is consumed yet. Kept named for the public API and future use.
    let _ = (config, progress);
    match destination {
        PushDestination::Http(remote_url) => push_http(
            git_dir,
            common_git_dir,
            format,
            remote_url,
            refspecs,
            options,
            credentials,
        ),
        PushDestination::Ssh(remote_url) => crate::ssh::push_ssh(
            git_dir,
            common_git_dir,
            format,
            remote_url,
            refspecs,
            options.quiet,
            options.force,
            credentials,
        ),
        PushDestination::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => push_local(
            git_dir,
            common_git_dir,
            format,
            remote,
            remote_git_dir,
            remote_common_git_dir,
            refspecs,
            options,
        ),
    }
}

/// Push to a smart-HTTP(S) remote: advertise via receive-pack info/refs, plan,
/// build the pack, POST the receive-pack RPC, and validate the report-status.
#[allow(clippy::too_many_arguments)]
fn push_http(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    remote_url: &RemoteUrl,
    refspecs: &[String],
    options: &PushOptions,
    credentials: &mut dyn CredentialProvider,
) -> Result<PushOutcome> {
    let client = crate::http::new_http_client();
    let advertisement_set = crate::http::http_service_advertisements(
        &client,
        remote_url,
        format,
        GitService::ReceivePack,
        credentials,
    )?;
    let features = advertised_receive_pack_features(&advertisement_set.refs)?;
    verify_remote_object_format(&features, format)?;

    let local_store = FileRefStore::new(git_dir, format);
    let local_refs = local_push_source_refs(&local_store, format)?;
    let command_forces = plan_push_command_forces(
        format,
        &local_refs,
        &advertisement_set.refs,
        refspecs,
        options.force,
    )?;
    let commands = commands_from_forces(&command_forces);
    if commands.is_empty() {
        return Ok(PushOutcome::default());
    }

    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    reject_non_fast_forward_pushes(&local_db, format, &command_forces)?;
    let packfile = build_push_packfile_against_advertisements(
        &local_db,
        format,
        &commands,
        &advertisement_set.refs,
    )?;
    let request = build_receive_pack_push_request(
        &features,
        commands.clone(),
        packfile,
        receive_pack_push_options(&features, format, options.quiet),
    )?;

    let mut body = Vec::new();
    write_receive_pack_push_request(&mut body, &request)?;
    let url = http_smart_rpc_url(remote_url, GitService::ReceivePack)?;
    let content_type = smart_http_rpc_request_content_type(GitService::ReceivePack)?;
    let mut response = crate::http::http_send_with_auth(remote_url, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &crate::http::http_authorization_headers(auth),
            body.clone(),
        )
    })?;
    crate::http::http_check_status(&response, &url)?;
    crate::http::http_validate_content_type(
        &response,
        &smart_http_rpc_result_content_type(GitService::ReceivePack)?,
    )?;

    let report = if features.report_status {
        let report = read_receive_pack_report_status(&mut response.body)?;
        validate_receive_pack_report(&report)?;
        Some(report)
    } else {
        let mut sink = Vec::new();
        response.body.read_to_end(&mut sink)?;
        None
    };
    Ok(PushOutcome { commands, report })
}

/// Push to a local repository served in-process: advertise from the remote
/// `git_dir`, plan, build the pack against the remote's reachable objects, and
/// apply the receive-pack request directly.
#[allow(clippy::too_many_arguments)]
fn push_local(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    remote_git_dir: &Path,
    remote_common_git_dir: &Path,
    refspecs: &[String],
    options: &PushOptions,
) -> Result<PushOutcome> {
    let _ = remote;
    let remote_format = crate::object_format_for_git_dir(remote_common_git_dir)?;
    if remote_format != format {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            remote_format.name(),
            format.name()
        )));
    }

    let local_store = FileRefStore::new(git_dir, format);
    let local_refs = local_push_source_refs(&local_store, format)?;
    let remote_refs = crate::local::local_fetch_advertisements(remote_git_dir, format)?;
    let command_forces =
        plan_push_command_forces(format, &local_refs, &remote_refs, refspecs, options.force)?;
    let commands = commands_from_forces(&command_forces);
    if commands.is_empty() {
        return Ok(PushOutcome::default());
    }

    let remote_excluded_tips = remote_refs
        .iter()
        .map(|reference| reference.oid.clone())
        .collect::<Vec<_>>();
    let starts = commands
        .iter()
        .filter(|command| !is_zero_object_id(&command.new_id))
        .map(|command| command.new_id.clone());
    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let remote_db = FileObjectDatabase::from_git_dir(remote_common_git_dir, format);
    reject_non_fast_forward_pushes(&local_db, format, &command_forces)?;
    let remote_excluded = collect_reachable_object_ids(&remote_db, format, remote_excluded_tips)?;
    let packfile = build_reachable_pack(&local_db, format, starts, &remote_excluded)?
        .map(|pack| pack.pack)
        .unwrap_or_default();
    let request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            shallow: Vec::new(),
            commands: commands.clone(),
            capabilities: Vec::new(),
        },
        push_options: None,
        packfile,
    };
    crate::local::receive_pack_into_local_repository(remote_git_dir, format, &request)?;
    Ok(PushOutcome {
        commands,
        report: None,
    })
}

/// Parse the receive-pack features from the leading ref advertisement (the empty
/// default when the remote advertised no refs).
fn advertised_receive_pack_features(
    advertisements: &[RefAdvertisement],
) -> Result<ReceivePackFeatures> {
    advertisements
        .first()
        .map(|advertisement| parse_receive_pack_features(&advertisement.capabilities))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Reject a push whose object format disagrees with the remote's advertised
/// `object-format`, and require the advertisement for any non-SHA-1 push.
fn verify_remote_object_format(features: &ReceivePackFeatures, format: ObjectFormat) -> Result<()> {
    if let Some(remote_format) = features.object_format {
        if remote_format != format {
            return Err(GitError::InvalidObjectId(format!(
                "remote repository uses {}, local repository uses {}",
                remote_format.name(),
                format.name()
            )));
        }
    } else if format != ObjectFormat::Sha1 {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository did not advertise object-format for {} push",
            format.name()
        )));
    }
    Ok(())
}

/// The receive-pack push-request options for the negotiated `features`, matching
/// git: report-status when advertised, ofs-delta when advertised, `quiet` only
/// when both requested and advertised, and the advertised object-format only when
/// the local repository's `format` is not SHA-1.
fn receive_pack_push_options(
    features: &ReceivePackFeatures,
    format: ObjectFormat,
    quiet: bool,
) -> ReceivePackPushRequestOptions {
    ReceivePackPushRequestOptions {
        report_status: features.report_status,
        ofs_delta: features.ofs_delta,
        quiet: quiet && features.quiet,
        object_format: features
            .object_format
            .filter(|_| format != ObjectFormat::Sha1),
        ..ReceivePackPushRequestOptions::default()
    }
}

/// Plan the receive-pack commands for `refspecs`, pairing each with whether it is
/// forced (the global `force` flag or the refspec's own `+`). Each refspec is
/// normalized then planned independently so per-refspec force is preserved,
/// matching the CLI.
fn plan_push_command_forces(
    format: ObjectFormat,
    local_refs: &[PushSourceRef],
    remote_refs: &[RefAdvertisement],
    refspecs: &[String],
    force: bool,
) -> Result<Vec<(ReceivePackCommand, bool)>> {
    let parsed_refspecs = refspecs
        .iter()
        .map(|refspec| parse_refspec(&normalize_push_refspec(refspec)))
        .collect::<Result<Vec<_>>>()?;
    let mut command_forces = Vec::new();
    for refspec in &parsed_refspecs {
        for command in plan_push_commands(
            format,
            local_refs,
            remote_refs,
            std::slice::from_ref(refspec),
        )? {
            command_forces.push((command, force || refspec.force));
        }
    }
    Ok(command_forces)
}

/// The planned commands, dropping the per-command force flags.
fn commands_from_forces(command_forces: &[(ReceivePackCommand, bool)]) -> Vec<ReceivePackCommand> {
    command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect()
}

/// Build the pack of objects the remote lacks, excluding everything reachable
/// from the advertised tips the local repository already has. Used by the HTTP
/// and SSH paths (the local path excludes via the remote's own object database).
fn build_push_packfile_against_advertisements(
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    commands: &[ReceivePackCommand],
    advertisements: &[RefAdvertisement],
) -> Result<Vec<u8>> {
    let remote_excluded_tips = remote_advertisement_tips_known_to_local(local_db, advertisements)?;
    let remote_excluded = collect_reachable_object_ids(local_db, format, remote_excluded_tips)?;
    let starts = commands
        .iter()
        .filter(|command| !is_zero_object_id(&command.new_id))
        .map(|command| command.new_id.clone());
    Ok(build_reachable_pack(local_db, format, starts, &remote_excluded)?
        .map(|pack| pack.pack)
        .unwrap_or_default())
}

/// The advertised tips the local repository already has, deduplicated and
/// excluding the all-zero sentinel — the safe negotiation base for the push pack.
pub fn remote_advertisement_tips_known_to_local(
    local_db: &FileObjectDatabase,
    advertisements: &[RefAdvertisement],
) -> Result<Vec<ObjectId>> {
    let mut tips = Vec::new();
    let mut seen = HashSet::new();
    for advertisement in advertisements {
        if is_zero_object_id(&advertisement.oid) || !seen.insert(advertisement.oid.clone()) {
            continue;
        }
        if local_db.contains(&advertisement.oid)? {
            tips.push(advertisement.oid.clone());
        }
    }
    Ok(tips)
}

/// Validate a receive-pack report-status, surfacing a failed unpack or any
/// rejected ref as an error (matching git's exit-failure message form).
pub fn validate_receive_pack_report(report: &ReceivePackReportStatus) -> Result<()> {
    if let ReceivePackUnpackStatus::Error(message) = &report.unpack {
        return Err(GitError::Command(format!(
            "failed to push some refs: unpack failed: {message}"
        )));
    }
    for status in &report.commands {
        if let ReceivePackCommandStatus::Ng { name, message } = status {
            return Err(GitError::Command(format!(
                "failed to push {name}: {message}"
            )));
        }
    }
    Ok(())
}

/// The push-source refs a local repository can match refspecs against: every ref
/// resolved to its object id, plus the short `refs/heads/`*and `refs/tags/`*
/// aliases, plus `HEAD`. Errors if any ref's object id does not match `format`.
pub fn local_push_source_refs(
    store: &FileRefStore,
    format: ObjectFormat,
) -> Result<Vec<PushSourceRef>> {
    let mut refs = Vec::new();
    for reference in store.list_refs()? {
        let Some((oid, _)) = resolve_for_each_ref_target(store, &reference)? else {
            continue;
        };
        if oid.format() != format {
            return Err(GitError::InvalidObjectId(format!(
                "local ref {} has {} object id for {} repository",
                reference.name,
                oid.format().name(),
                format.name()
            )));
        }
        refs.push(PushSourceRef {
            name: reference.name.clone(),
            oid: oid.clone(),
        });
        if let Some(short) = reference.name.strip_prefix("refs/heads/") {
            refs.push(PushSourceRef {
                name: short.to_string(),
                oid: oid.clone(),
            });
        }
        if let Some(short) = reference.name.strip_prefix("refs/tags/") {
            refs.push(PushSourceRef {
                name: short.to_string(),
                oid,
            });
        }
    }
    if let Some(target) = store.read_ref("HEAD")? {
        let head = Ref {
            name: "HEAD".to_string(),
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(store, &head)?
            && oid.format() == format
        {
            refs.push(PushSourceRef {
                name: "HEAD".to_string(),
                oid,
            });
        }
    }
    Ok(refs)
}

/// Normalize a push refspec, expanding short names to `refs/heads/<name>` on both
/// sides and supplying the source as the destination when none is given, while
/// preserving a leading `+` force marker.
pub fn normalize_push_refspec(refspec: &str) -> String {
    let (force, refspec) = refspec
        .strip_prefix('+')
        .map_or((false, refspec), |refspec| (true, refspec));
    let normalized = if let Some((src, dst)) = refspec.split_once(':') {
        let src = normalize_push_refname(src);
        let dst = normalize_push_refname(dst);
        format!("{src}:{dst}")
    } else {
        let name = normalize_push_refname(refspec);
        format!("{name}:{name}")
    };
    if force {
        format!("+{normalized}")
    } else {
        normalized
    }
}

/// Expand a short push ref name to `refs/heads/<name>`, leaving empty names,
/// `HEAD`, and already-qualified `refs/`* names untouched.
pub fn normalize_push_refname(name: &str) -> String {
    if name.is_empty() || name == "HEAD" || name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    }
}

/// Reject any non-forced branch update whose old tip is not an ancestor of the
/// new tip (a non-fast-forward). Forced updates, non-branch refs, and
/// creations/deletions are skipped.
pub fn reject_non_fast_forward_pushes(
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    command_forces: &[(ReceivePackCommand, bool)],
) -> Result<()> {
    for (command, force) in command_forces {
        if *force
            || !command.name.starts_with("refs/heads/")
            || is_zero_object_id(&command.old_id)
            || is_zero_object_id(&command.new_id)
        {
            continue;
        }
        let ancestors = ancestor_depths(local_db, format, &command.new_id)?;
        if !ancestors.contains_key(&command.old_id) {
            let short = command.name.trim_start_matches("refs/heads/");
            return Err(GitError::Command(format!(
                "failed to push some refs: non-fast-forward update to {short}"
            )));
        }
    }
    Ok(())
}

/// The depth of every commit reachable from `start` (a breadth-first ancestry
/// walk). Used to test fast-forwardness: `start`'s ancestors include `start`
/// itself at depth zero. Errors if a reachable object is not a commit.
fn ancestor_depths(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    start: &ObjectId,
) -> Result<HashMap<ObjectId, usize>> {
    let mut depths = HashMap::new();
    let mut pending = std::collections::VecDeque::from([(start.clone(), 0usize)]);
    while let Some((oid, depth)) = pending.pop_front() {
        if depths.get(&oid).is_some_and(|existing| *existing <= depth) {
            continue;
        }
        depths.insert(oid.clone(), depth);
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse(format, &object.body)?;
        for parent in commit.parents {
            pending.push_back((parent, depth + 1));
        }
    }
    Ok(depths)
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
