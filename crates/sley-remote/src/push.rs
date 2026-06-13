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

use crate::pack::push_pack_roots;
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

/// One caller-authored receive-pack command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCommand {
    /// The object id to install at `dst`, or `None` for a delete.
    pub src: Option<ObjectId>,
    /// Full destination ref name.
    pub dst: String,
    /// The expected remote old object id. `None` lowers to the zero oid, which
    /// receive-pack treats as create-only for updates and unconditional for
    /// deletes.
    pub expected_old: Option<ObjectId>,
}

/// A typed push action that preserves the caller's exact old/new/delete intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushAction {
    Create {
        dst: String,
        new: ObjectId,
    },
    Update {
        dst: String,
        old: ObjectId,
        new: ObjectId,
    },
    Delete {
        dst: String,
        old: Option<ObjectId>,
    },
}

impl From<PushAction> for PushCommand {
    fn from(value: PushAction) -> Self {
        match value {
            PushAction::Create { dst, new } => Self {
                src: Some(new),
                dst,
                expected_old: None,
            },
            PushAction::Update { dst, old, new } => Self {
                src: Some(new),
                dst,
                expected_old: Some(old),
            },
            PushAction::Delete { dst, old } => Self {
                src: None,
                dst,
                expected_old: old,
            },
        }
    }
}

/// A caller-authored push plan. This is distinct from [`PushPlan`], which is a
/// negotiated, executable transport token returned by [`plan_push`].
#[derive(Debug, Clone)]
pub struct PushActionPlan {
    pub commands: Vec<PushCommand>,
    pub pack_objects: Vec<ObjectId>,
    pub options: PushOptions,
}

impl PushActionPlan {
    pub fn from_actions(actions: Vec<PushAction>, options: PushOptions) -> Self {
        Self {
            commands: actions.into_iter().map(PushCommand::from).collect(),
            pack_objects: Vec::new(),
            options,
        }
    }
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

/// Fully resolved inputs for a caller-authored exact push plan.
#[derive(Clone, Copy)]
pub struct PushActionRequest<'a> {
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
    /// Caller-authored exact push plan.
    pub plan: &'a PushActionPlan,
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
        pack_objects: Vec<ObjectId>,
    },
    Ssh(crate::ssh::SshPushPlan),
    Local {
        remote_git_dir: PathBuf,
        remote_common_git_dir: PathBuf,
        remote_refs: Vec<RefAdvertisement>,
        command_forces: Vec<(ReceivePackCommand, bool)>,
        pack_objects: Vec<ObjectId>,
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

/// Push a caller-authored exact plan, preserving its old/new/delete command ids.
pub fn push_actions(
    request: PushActionRequest<'_>,
    mut services: PushServices<'_>,
) -> Result<PushOutcome> {
    let plan = plan_push_actions(request, &mut services)?;
    execute_push_action_plan(request, &mut services, plan)
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

/// Negotiate with the remote and bind a caller-authored exact push plan to a
/// transport execution token.
pub fn plan_push_actions(
    request: PushActionRequest<'_>,
    services: &mut PushServices<'_>,
) -> Result<PushPlan> {
    let _ = request.config;
    let _ = &mut services.progress;
    let commands = receive_pack_commands_from_action_plan(request.format, request.plan)?;
    let command_forces = commands
        .iter()
        .cloned()
        .map(|command| (command, request.plan.options.force))
        .collect::<Vec<_>>();
    match request.destination {
        #[cfg(feature = "http")]
        PushDestination::Http(remote_url) => {
            let client = crate::http::new_http_client();
            let discovered = crate::http::http_service_advertisements(
                &client,
                remote_url,
                request.format,
                GitService::ReceivePack,
                services.credentials,
            )?;
            let advertisement_set = discovered.set;
            let features = advertised_receive_pack_features(&advertisement_set.refs)?;
            verify_remote_object_format(&features, request.format)?;
            let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
            reject_non_fast_forward_pushes(&local_db, request.format, &command_forces)?;
            let execution = if commands.is_empty() {
                PushExecution::Noop
            } else {
                PushExecution::Http {
                    remote_url: remote_url.clone(),
                    features,
                    advertisements: advertisement_set.refs,
                    command_forces,
                    pack_objects: request.plan.pack_objects.clone(),
                }
            };
            Ok(PushPlan {
                commands,
                execution,
            })
        }
        #[cfg(not(feature = "http"))]
        PushDestination::Http(_) => Err(GitError::Unsupported(
            "HTTP transport is not enabled in this build".into(),
        )),
        PushDestination::Ssh(remote_url) => {
            let plan = crate::ssh::plan_push_ssh_commands(crate::ssh::SshPushCommandsRequest {
                common_git_dir: request.common_git_dir,
                format: request.format,
                remote: remote_url,
                commands: commands.clone(),
                pack_objects: request.plan.pack_objects.clone(),
                force: request.plan.options.force,
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
        } => {
            let remote_format = crate::object_format_for_git_dir(remote_common_git_dir)?;
            if remote_format != request.format {
                return Err(GitError::InvalidObjectId(format!(
                    "remote repository uses {}, local repository uses {}",
                    remote_format.name(),
                    request.format.name()
                )));
            }
            let remote_refs =
                crate::local::local_fetch_advertisements(remote_git_dir, request.format)?;
            let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
            reject_non_fast_forward_pushes(&local_db, request.format, &command_forces)?;
            let execution = if commands.is_empty() {
                PushExecution::Noop
            } else {
                PushExecution::Local {
                    remote_git_dir: remote_git_dir.to_path_buf(),
                    remote_common_git_dir: remote_common_git_dir.to_path_buf(),
                    remote_refs,
                    command_forces,
                    pack_objects: request.plan.pack_objects.clone(),
                }
            };
            Ok(PushPlan {
                commands,
                execution,
            })
        }
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
            pack_objects,
        } => execute_push_http(
            request,
            services.credentials,
            plan.commands,
            remote_url,
            features,
            advertisements,
            command_forces,
            pack_objects,
        ),
        PushExecution::Ssh(plan) => crate::ssh::execute_push_ssh_plan(request, plan),
        PushExecution::Local {
            remote_git_dir,
            remote_common_git_dir,
            remote_refs,
            command_forces,
            pack_objects,
        } => execute_push_local(
            request,
            plan.commands,
            remote_git_dir,
            remote_common_git_dir,
            remote_refs,
            command_forces,
            pack_objects,
        ),
    }
}

/// Execute a previously negotiated exact push plan.
pub fn execute_push_action_plan(
    request: PushActionRequest<'_>,
    services: &mut PushServices<'_>,
    plan: PushPlan,
) -> Result<PushOutcome> {
    let refspecs: &[String] = &[];
    execute_push_plan(
        PushRequest {
            git_dir: request.git_dir,
            common_git_dir: request.common_git_dir,
            format: request.format,
            config: request.config,
            remote: request.remote,
            destination: request.destination,
            refspecs,
            options: &request.plan.options,
        },
        services,
        plan,
    )
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
    let mut local_refs = local_push_source_refs(&local_store, format)?;
    add_revision_push_sources(git_dir, format, refspecs, &mut local_refs);
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
            pack_objects: Vec::new(),
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
    pack_objects: Vec<ObjectId>,
) -> Result<PushOutcome> {
    let client = crate::http::new_http_client();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let body = build_receive_pack_body(&PushPackRequest {
        local_db: &local_db,
        format: request.format,
        commands: &commands,
        pack_objects: &pack_objects,
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
    let mut local_refs = local_push_source_refs(&local_store, format)?;
    add_revision_push_sources(git_dir, format, refspecs, &mut local_refs);
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
            pack_objects: Vec::new(),
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
    pack_objects: Vec<ObjectId>,
) -> Result<PushOutcome> {
    let remote_excluded_tips = remote_refs
        .iter()
        .map(|reference| reference.oid)
        .collect::<Vec<_>>();
    let starts = push_pack_roots(&commands, &pack_objects);
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
    let report = crate::local::receive_pack_reachable_pack_into_local_repository(
        &remote_git_dir,
        request.format,
        &receive_request,
        &local_db,
        starts,
        remote_excluded,
    )?;
    validate_receive_pack_report(&report)?;
    Ok(PushOutcome {
        commands,
        report: Some(report),
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
        .map(|refspec| parse_refspec(&normalize_push_refspec_for_sources(refspec, local_refs)))
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

fn add_revision_push_sources(
    git_dir: &Path,
    format: ObjectFormat,
    refspecs: &[String],
    local_refs: &mut Vec<PushSourceRef>,
) {
    for refspec in refspecs {
        let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
        let src = refspec.split_once(':').map_or(refspec, |(src, _)| src);
        if src.is_empty() || src == "HEAD" || src.starts_with("refs/") {
            continue;
        }
        if local_refs.iter().any(|reference| {
            reference.name == src
                || reference.name == format!("refs/heads/{src}")
                || reference.name == format!("refs/tags/{src}")
        }) {
            continue;
        }
        if let Ok(oid) = sley_rev::resolve_revision(git_dir, format, src)
            && !local_refs.iter().any(|reference| reference.name == src)
        {
            local_refs.push(PushSourceRef {
                name: src.to_string(),
                oid,
            });
        }
    }
}

fn normalize_push_refspec_for_sources(refspec: &str, local_refs: &[PushSourceRef]) -> String {
    let (force, refspec) = refspec
        .strip_prefix('+')
        .map_or((false, refspec), |refspec| (true, refspec));
    let normalized = if let Some((src, dst)) = refspec.split_once(':') {
        let (src, src_kind) = normalize_push_source_refname(src, local_refs);
        let dst = normalize_push_destination_refname(dst, src_kind);
        format!("{src}:{dst}")
    } else {
        let (name, _) = normalize_push_source_refname(refspec, local_refs);
        format!("{name}:{name}")
    };
    if force {
        format!("+{normalized}")
    } else {
        normalized
    }
}

#[derive(Clone, Copy)]
enum PushSourceKind {
    Branch,
    Tag,
    Other,
}

fn normalize_push_source_refname(
    name: &str,
    local_refs: &[PushSourceRef],
) -> (String, PushSourceKind) {
    if name.is_empty() || name == "HEAD" || name.starts_with("refs/") {
        return (name.to_string(), PushSourceKind::Other);
    }
    let branch = format!("refs/heads/{name}");
    let tag = format!("refs/tags/{name}");
    let has_branch = local_refs.iter().any(|reference| reference.name == branch);
    let has_tag = local_refs.iter().any(|reference| reference.name == tag);
    if has_tag && !has_branch {
        (tag, PushSourceKind::Tag)
    } else if has_branch {
        (branch, PushSourceKind::Branch)
    } else if local_refs.iter().any(|reference| reference.name == name) {
        (name.to_string(), PushSourceKind::Other)
    } else {
        (branch, PushSourceKind::Branch)
    }
}

fn normalize_push_destination_refname(name: &str, src_kind: PushSourceKind) -> String {
    if name.is_empty() || name == "HEAD" || name.starts_with("refs/") {
        return name.to_string();
    }
    match src_kind {
        PushSourceKind::Tag => format!("refs/tags/{name}"),
        PushSourceKind::Branch | PushSourceKind::Other => format!("refs/heads/{name}"),
    }
}

/// The planned commands, dropping the per-command force flags.
fn commands_from_forces(command_forces: &[(ReceivePackCommand, bool)]) -> Vec<ReceivePackCommand> {
    command_forces
        .iter()
        .map(|(command, _)| command.clone())
        .collect()
}

fn receive_pack_commands_from_action_plan(
    format: ObjectFormat,
    plan: &PushActionPlan,
) -> Result<Vec<ReceivePackCommand>> {
    let zero = ObjectId::null(format);
    for oid in &plan.pack_objects {
        if oid.format() != format {
            return Err(GitError::InvalidObjectId(format!(
                "push pack object {oid} has {} object id for {} repository",
                oid.format().name(),
                format.name()
            )));
        }
    }
    plan.commands
        .iter()
        .map(|command| {
            let old_id = command.expected_old.unwrap_or(zero);
            let new_id = command.src.unwrap_or(zero);
            if old_id.format() != format {
                return Err(GitError::InvalidObjectId(format!(
                    "push command {} expected old has {} object id for {} repository",
                    command.dst,
                    old_id.format().name(),
                    format.name()
                )));
            }
            if new_id.format() != format {
                return Err(GitError::InvalidObjectId(format!(
                    "push command {} new id has {} object id for {} repository",
                    command.dst,
                    new_id.format().name(),
                    format.name()
                )));
            }
            Ok(ReceivePackCommand {
                old_id,
                new_id,
                name: command.dst.clone(),
            })
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sley_formats::RepositoryLayout;
    use sley_object::{Commit, EncodedObject, ObjectType, Tree};
    use sley_odb::{FileObjectDatabase, ObjectWriter};
    use sley_protocol::{ReceivePackCommandStatus, ReceivePackUnpackStatus};
    use sley_refs::{RefTarget, RefUpdate};

    use crate::{NoCredentials, SilentProgress};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-remote-push-{name}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        RepositoryLayout::init_at(&dir, ObjectFormat::Sha1, false)
            .expect("test repository should initialize");
        dir.join(".git")
    }

    fn write_commit(git_dir: &Path, parents: Vec<ObjectId>, message: &str) -> ObjectId {
        let format = ObjectFormat::Sha1;
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        let tree = db
            .write_object(EncodedObject::new(
                ObjectType::Tree,
                Tree { entries: vec![] }.write(),
            ))
            .expect("tree should write");
        let identity = b"Test User <test@example.invalid> 1 +0000".to_vec();
        db.write_object(EncodedObject::new(
            ObjectType::Commit,
            Commit {
                tree,
                parents,
                author: identity.clone(),
                committer: identity,
                encoding: None,
                message: format!("{message}\n").into_bytes(),
            }
            .write(),
        ))
        .expect("commit should write")
    }

    fn set_ref(git_dir: &Path, name: &str, target: RefTarget) {
        let store = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: name.to_string(),
            expected: None,
            new: target,
            reflog: None,
        });
        tx.commit().expect("ref should update");
    }

    fn default_options() -> PushOptions {
        PushOptions {
            quiet: true,
            force: false,
        }
    }

    fn push_local_actions(
        local: &Path,
        remote: &Path,
        plan: &PushActionPlan,
    ) -> Result<PushOutcome> {
        let destination = PushDestination::Local {
            git_dir: remote.to_path_buf(),
            common_git_dir: remote.to_path_buf(),
        };
        let config = GitConfig::default();
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;
        push_actions(
            PushActionRequest {
                git_dir: local,
                common_git_dir: local,
                format: ObjectFormat::Sha1,
                config: &config,
                remote: "origin",
                destination: &destination,
                plan,
            },
            PushServices {
                credentials: &mut credentials,
                progress: &mut progress,
            },
        )
    }

    #[test]
    fn local_push_returns_success_report_status_and_updates_ref() {
        let local = temp_repo("local-success");
        let remote = temp_repo("remote-success");
        let base = write_commit(&local, Vec::new(), "base");
        let tip = write_commit(&local, vec![base], "tip");
        set_ref(&local, "refs/heads/main", RefTarget::Direct(tip));
        set_ref(
            &local,
            "HEAD",
            RefTarget::Symbolic("refs/heads/main".into()),
        );
        let destination = PushDestination::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let refspecs = vec!["refs/heads/main:refs/heads/main".to_string()];
        let options = default_options();
        let request = PushRequest {
            git_dir: &local,
            common_git_dir: &local,
            format: ObjectFormat::Sha1,
            config: &GitConfig::default(),
            remote: "origin",
            destination: &destination,
            refspecs: &refspecs,
            options: &options,
        };
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;

        let outcome = push(
            request,
            PushServices {
                credentials: &mut credentials,
                progress: &mut progress,
            },
        )
        .expect("push should succeed");

        assert_eq!(outcome.commands.len(), 1);
        let report = outcome.report.expect("local receive-pack reports status");
        assert!(matches!(report.unpack, ReceivePackUnpackStatus::Ok));
        assert!(matches!(
            report.commands.as_slice(),
            [ReceivePackCommandStatus::Ok { name }] if name == "refs/heads/main"
        ));
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(tip))
        );
    }

    #[test]
    fn local_push_actions_preserves_exact_old_new_update() {
        let local = temp_repo("actions-update-local");
        let remote = temp_repo("actions-update-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let tip = write_commit(&local, vec![base], "tip");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(base));
        let plan = PushActionPlan::from_actions(
            vec![PushAction::Update {
                dst: "refs/heads/main".into(),
                old: base,
                new: tip,
            }],
            default_options(),
        );

        let outcome = push_local_actions(&local, &remote, &plan).expect("push actions");

        assert_eq!(outcome.commands.len(), 1);
        assert_eq!(outcome.commands[0].old_id, base);
        assert_eq!(outcome.commands[0].new_id, tip);
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(tip))
        );
    }

    #[test]
    fn local_push_actions_stale_update_old_rejects_without_mutating() {
        let local = temp_repo("actions-stale-local");
        let remote = temp_repo("actions-stale-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let tip = write_commit(&local, vec![base], "tip");
        let concurrent = write_commit(&remote, vec![base], "concurrent");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(concurrent));
        let plan = PushActionPlan::from_actions(
            vec![PushAction::Update {
                dst: "refs/heads/main".into(),
                old: base,
                new: tip,
            }],
            default_options(),
        );

        let err = push_local_actions(&local, &remote, &plan).expect_err("stale old rejects");

        assert!(err.to_string().contains("expected ref refs/heads/main"));
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(concurrent))
        );
    }

    #[test]
    fn local_push_actions_stale_delete_old_rejects_without_mutating() {
        let local = temp_repo("actions-delete-local");
        let remote = temp_repo("actions-delete-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let concurrent = write_commit(&remote, vec![base], "concurrent");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(concurrent));
        let plan = PushActionPlan::from_actions(
            vec![PushAction::Delete {
                dst: "refs/heads/main".into(),
                old: Some(base),
            }],
            default_options(),
        );

        let err = push_local_actions(&local, &remote, &plan).expect_err("stale delete rejects");

        assert!(err.to_string().contains("expected ref refs/heads/main"));
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(concurrent))
        );
    }

    #[test]
    fn local_push_actions_create_rejects_existing_ref() {
        let local = temp_repo("actions-create-local");
        let remote = temp_repo("actions-create-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let tip = write_commit(&local, vec![base], "tip");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(base));
        let plan = PushActionPlan::from_actions(
            vec![PushAction::Create {
                dst: "refs/heads/main".into(),
                new: tip,
            }],
            default_options(),
        );

        let err = push_local_actions(&local, &remote, &plan).expect_err("create must be absent");

        assert!(
            err.to_string()
                .contains("expected ref refs/heads/main to not already exist")
        );
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(base))
        );
    }

    #[test]
    fn report_status_rejection_is_an_error() {
        let report = ReceivePackReportStatus {
            unpack: ReceivePackUnpackStatus::Ok,
            commands: vec![ReceivePackCommandStatus::Ng {
                name: "refs/heads/main".into(),
                message: "hook declined".into(),
            }],
        };

        let err = validate_receive_pack_report(&report).expect_err("ng report should fail");

        assert!(err.to_string().contains("hook declined"));
    }

    #[test]
    fn failed_local_push_does_not_partially_mutate_remote_ref() {
        let local = temp_repo("local-rejected");
        let remote = temp_repo("remote-rejected");
        let base = write_commit(&local, Vec::new(), "base");
        let planned = write_commit(&local, vec![base], "planned");
        let concurrent = write_commit(&local, vec![base], "concurrent");
        set_ref(&local, "refs/heads/main", RefTarget::Direct(planned));
        set_ref(
            &local,
            "HEAD",
            RefTarget::Symbolic("refs/heads/main".into()),
        );
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(base));
        let destination = PushDestination::Local {
            git_dir: remote.clone(),
            common_git_dir: remote.clone(),
        };
        let refspecs = vec!["refs/heads/main:refs/heads/main".to_string()];
        let options = default_options();
        let request = PushRequest {
            git_dir: &local,
            common_git_dir: &local,
            format: ObjectFormat::Sha1,
            config: &GitConfig::default(),
            remote: "origin",
            destination: &destination,
            refspecs: &refspecs,
            options: &options,
        };
        let mut credentials = NoCredentials;
        let mut progress = SilentProgress;
        let mut services = PushServices {
            credentials: &mut credentials,
            progress: &mut progress,
        };
        let plan = plan_push(request, &mut services).expect("push should plan");

        set_ref(&remote, "refs/heads/main", RefTarget::Direct(concurrent));
        let _err = execute_push_plan(request, &mut services, plan)
            .expect_err("stale old id should reject the ref update");

        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(concurrent))
        );
    }
}
