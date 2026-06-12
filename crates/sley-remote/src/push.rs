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

use std::collections::HashMap;
#[cfg(feature = "http")]
use std::io::Read;
use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader, collect_reachable_object_ids};
#[cfg(feature = "http")]
use sley_protocol::{
    GitService, ReceivePackFeatures, ReceivePackPushRequestOptions, parse_receive_pack_features,
    read_receive_pack_report_status, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type,
};
use sley_protocol::{
    PushSourceRef, ReceivePackCommand, ReceivePackCommandStatus, ReceivePackPushRequest,
    ReceivePackReportStatus, ReceivePackRequest, ReceivePackUnpackStatus, RefAdvertisement,
    parse_refspec, plan_push_commands,
};

#[cfg(feature = "http")]
use crate::pack::{PushPackRequest, build_receive_pack_body};
use sley_refs::{FileRefStore, Ref, RefTarget};
use sley_transport::RemoteUrl;
#[cfg(feature = "http")]
use sley_transport::{HttpClient, http_smart_rpc_url};

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

/// Fully resolved inputs for a [`push`] run.
#[derive(Clone, Copy)]
pub struct PushRequest<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository common `$GIT_DIR`, used for object access.
    pub common_git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// Local repository config snapshot.
    pub config: &'a GitConfig,
    /// Remote name or source string, used for diagnostics.
    pub remote: &'a str,
    /// Already-resolved push destination.
    pub destination: &'a PushDestination,
    /// Refspecs requested by the caller.
    pub refspecs: &'a [String],
    /// Push behavior flags.
    pub options: &'a PushOptions,
}

/// Mutable seams used while pushing.
pub struct PushServices<'a> {
    /// Credential source for authenticated transports.
    pub credentials: &'a mut dyn CredentialProvider,
    /// Progress sink reserved for future push progress.
    pub progress: &'a mut dyn ProgressSink,
}

/// A push after ref negotiation and command planning, but before any ref update
/// is sent or applied.
pub struct PushPlan {
    /// The receive-pack commands that will be executed if the caller proceeds.
    pub commands: Vec<ReceivePackCommand>,
    execution: PushExecution,
}

enum PushExecution {
    Noop,
    #[cfg(feature = "http")]
    Http {
        remote_url: RemoteUrl,
        features: ReceivePackFeatures,
        advertisements: Vec<RefAdvertisement>,
        command_forces: Vec<(ReceivePackCommand, bool)>,
    },
    Ssh(crate::ssh::SshPushPlan),
    Local {
        remote_git_dir: PathBuf,
        remote_common_git_dir: PathBuf,
        remote_refs: Vec<RefAdvertisement>,
        command_forces: Vec<(ReceivePackCommand, bool)>,
    },
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
pub fn push(request: PushRequest<'_>, mut services: PushServices<'_>) -> Result<PushOutcome> {
    let plan = plan_push(request, &mut services)?;
    execute_push_plan(request, &mut services, plan)
}

/// Negotiate with the remote and compute the receive-pack command list without
/// sending a pack or applying a ref update.
pub fn plan_push(request: PushRequest<'_>, services: &mut PushServices<'_>) -> Result<PushPlan> {
    // `config` and `progress` are part of the seam (mirroring `fetch`) but the
    // current push flow drives credentials from the caller-built provider and
    // returns its summary in `PushOutcome` rather than streaming progress, so
    // neither is consumed yet. Kept named for the public API and future use.
    let _ = request.config;
    let _ = &mut services.progress;
    match request.destination {
        #[cfg(feature = "http")]
        PushDestination::Http(remote_url) => plan_push_http(PushHttpRequest {
            git_dir: request.git_dir,
            common_git_dir: request.common_git_dir,
            format: request.format,
            remote_url,
            refspecs: request.refspecs,
            options: request.options,
            credentials: services.credentials,
        }),
        #[cfg(not(feature = "http"))]
        PushDestination::Http(_) => Err(GitError::Unsupported(
            "HTTP transport is not enabled in this build".into(),
        )),
        PushDestination::Ssh(remote_url) => {
            let plan = crate::ssh::plan_push_ssh(crate::ssh::SshPushRequest {
                git_dir: request.git_dir,
                common_git_dir: request.common_git_dir,
                format: request.format,
                remote: remote_url,
                refspecs: request.refspecs,
                force: request.options.force,
            })?;
            let commands = plan.commands.clone();
            let execution = if commands.is_empty() {
                PushExecution::Noop
            } else {
                PushExecution::Ssh(plan)
            };
            Ok(PushPlan {
                commands,
                execution,
            })
        }
        PushDestination::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => plan_push_local(PushLocalRequest {
            git_dir: request.git_dir,
            common_git_dir: request.common_git_dir,
            format: request.format,
            remote: request.remote,
            remote_git_dir,
            remote_common_git_dir,
            refspecs: request.refspecs,
            options: request.options,
        }),
    }
}

/// Execute a previously planned push.
pub fn execute_push_plan(
    request: PushRequest<'_>,
    services: &mut PushServices<'_>,
    plan: PushPlan,
) -> Result<PushOutcome> {
    let _ = (request.config, request.remote);
    let _ = &mut services.progress;
    if plan.commands.is_empty() {
        return Ok(PushOutcome::default());
    }
    match plan.execution {
        PushExecution::Noop => Ok(PushOutcome::default()),
        #[cfg(feature = "http")]
        PushExecution::Http {
            remote_url,
            features,
            advertisements,
            command_forces,
        } => execute_push_http(
            request,
            services.credentials,
            plan.commands,
            remote_url,
            features,
            advertisements,
            command_forces,
        ),
        PushExecution::Ssh(plan) => crate::ssh::execute_push_ssh_plan(request, plan),
        PushExecution::Local {
            remote_git_dir,
            remote_common_git_dir,
            remote_refs,
            command_forces,
        } => execute_push_local(
            request,
            plan.commands,
            remote_git_dir,
            remote_common_git_dir,
            remote_refs,
            command_forces,
        ),
    }
}

/// Push to a smart-HTTP(S) remote: advertise via receive-pack info/refs, plan,
/// build the pack, POST the receive-pack RPC, and validate the report-status.
#[cfg(feature = "http")]
struct PushHttpRequest<'a> {
    git_dir: &'a Path,
    common_git_dir: &'a Path,
    format: ObjectFormat,
    remote_url: &'a RemoteUrl,
    refspecs: &'a [String],
    options: &'a PushOptions,
    credentials: &'a mut dyn CredentialProvider,
}

#[cfg(feature = "http")]
fn plan_push_http(request: PushHttpRequest<'_>) -> Result<PushPlan> {
    let PushHttpRequest {
        git_dir,
        common_git_dir,
        format,
        remote_url,
        refspecs,
        options,
        credentials,
    } = request;
    let client = crate::http::new_http_client();
    let discovered = crate::http::http_service_advertisements(
        &client,
        remote_url,
        format,
        GitService::ReceivePack,
        credentials,
    )?;
    let advertisement_set = discovered.set;
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
    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    reject_non_fast_forward_pushes(&local_db, format, &command_forces)?;
    let commands = commands_from_forces(&command_forces);
    let execution = if commands.is_empty() {
        PushExecution::Noop
    } else {
        PushExecution::Http {
            remote_url: remote_url.clone(),
            features,
            advertisements: advertisement_set.refs,
            command_forces,
        }
    };
    Ok(PushPlan {
        commands,
        execution,
    })
}

#[cfg(feature = "http")]
fn execute_push_http(
    request: PushRequest<'_>,
    credentials: &mut dyn CredentialProvider,
    commands: Vec<ReceivePackCommand>,
    remote_url: RemoteUrl,
    features: ReceivePackFeatures,
    advertisements: Vec<RefAdvertisement>,
    _command_forces: Vec<(ReceivePackCommand, bool)>,
) -> Result<PushOutcome> {
    let client = crate::http::new_http_client();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let body = build_receive_pack_body(&PushPackRequest {
        local_db: &local_db,
        format: request.format,
        commands: &commands,
        remote_advertisements: &advertisements,
        features: &features,
        options: receive_pack_push_options(&features, request.format, request.options.quiet),
        thin: false,
    })?;
    let url = http_smart_rpc_url(&remote_url, GitService::ReceivePack)?;
    let content_type = smart_http_rpc_request_content_type(GitService::ReceivePack)?;
    let mut response = crate::http::http_send_with_auth(&remote_url, credentials, |auth| {
        client.post(
            &url,
            &content_type,
            &crate::http::http_authorization_headers(auth),
            &body,
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
struct PushLocalRequest<'a> {
    git_dir: &'a Path,
    common_git_dir: &'a Path,
    format: ObjectFormat,
    remote: &'a str,
    remote_git_dir: &'a Path,
    remote_common_git_dir: &'a Path,
    refspecs: &'a [String],
    options: &'a PushOptions,
}

fn plan_push_local(request: PushLocalRequest<'_>) -> Result<PushPlan> {
    let PushLocalRequest {
        git_dir,
        common_git_dir,
        format,
        remote,
        remote_git_dir,
        remote_common_git_dir,
        refspecs,
        options,
    } = request;
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
    let local_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    reject_non_fast_forward_pushes(&local_db, format, &command_forces)?;
    let commands = commands_from_forces(&command_forces);
    let execution = if commands.is_empty() {
        PushExecution::Noop
    } else {
        PushExecution::Local {
            remote_git_dir: remote_git_dir.to_path_buf(),
            remote_common_git_dir: remote_common_git_dir.to_path_buf(),
            remote_refs,
            command_forces,
        }
    };
    Ok(PushPlan {
        commands,
        execution,
    })
}

fn execute_push_local(
    request: PushRequest<'_>,
    commands: Vec<ReceivePackCommand>,
    remote_git_dir: PathBuf,
    remote_common_git_dir: PathBuf,
    remote_refs: Vec<RefAdvertisement>,
    _command_forces: Vec<(ReceivePackCommand, bool)>,
) -> Result<PushOutcome> {
    let remote_excluded_tips = remote_refs
        .iter()
        .map(|reference| reference.oid)
        .collect::<Vec<_>>();
    let starts = commands
        .iter()
        .filter(|command| !command.new_id.is_null())
        .map(|command| command.new_id.clone())
        .collect::<Vec<_>>();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let remote_db = FileObjectDatabase::from_git_dir(&remote_common_git_dir, request.format);
    let remote_excluded =
        collect_reachable_object_ids(&remote_db, request.format, remote_excluded_tips)?;
    let packfile = if starts.is_empty() {
        Vec::new()
    } else {
        b"PACK".to_vec()
    };
    let receive_request = ReceivePackPushRequest {
        commands: ReceivePackRequest {
            shallow: Vec::new(),
            commands: commands.clone(),
            capabilities: Vec::new(),
        },
        push_options: None,
        packfile,
    };
    crate::local::receive_pack_reachable_pack_into_local_repository(
        &remote_git_dir,
        request.format,
        &receive_request,
        &local_db,
        starts,
        remote_excluded,
    )?;
    Ok(PushOutcome {
        commands,
        report: None,
    })
}

/// Parse the receive-pack features from the leading ref advertisement (the empty
/// default when the remote advertised no refs).
#[cfg(feature = "http")]
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
#[cfg(feature = "http")]
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
#[cfg(feature = "http")]
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
            oid,
        });
        if let Some(short) = reference.name.strip_prefix("refs/heads/") {
            refs.push(PushSourceRef {
                name: short.to_string(),
                oid,
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
            || command.old_id.is_null()
            || command.new_id.is_null()
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
        depths.insert(oid, depth);
        let object = db.read_object(&oid)?;
        if object.object_type != ObjectType::Commit {
            return Err(GitError::InvalidObject(format!(
                "expected commit {oid}, found {}",
                object.object_type.as_str()
            )));
        }
        let commit = Commit::parse_ref(format, &object.body)?;
        for parent in commit.parents {
            pending.push_back((parent, depth + 1));
        }
    }
    Ok(depths)
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
