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

use std::fs;
#[cfg(feature = "http")]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result, redact_url_for_display};
use sley_object::ObjectType;
use sley_odb::{
    FileObjectDatabase, ObjectReader, RawPackInstallOptions, build_and_install_reachable_pack,
    collect_reachable_object_ids,
};
#[cfg(feature = "http")]
use sley_protocol::{
    GitService, ReceivePackFeatures, ReceivePackPushRequestOptions, parse_receive_pack_features,
    read_receive_pack_report_status, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type,
};
use sley_protocol::{
    PushSourceRef, ReceivePackCommand, ReceivePackCommandStatus, ReceivePackPushRequest,
    ReceivePackReportStatus, ReceivePackRequest, ReceivePackUnpackStatus, RefAdvertisement,
    RefSpec, parse_refspec, plan_push_commands,
};

use crate::pack::push_pack_roots;
#[cfg(feature = "http")]
use crate::pack::{PushPackRequest, write_receive_pack_body};
use sley_refs::{FileRefStore, Ref, RefTarget};
use sley_transport::RemoteUrl;
#[cfg(feature = "http")]
use sley_transport::{HttpClient, HttpResponse, http_smart_rpc_url};

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
    /// A native anonymous `git://` remote at the given already-resolved URL.
    Git(RemoteUrl),
    /// A local repository served in-process from `git_dir`.
    Local {
        /// The remote repository's `$GIT_DIR`.
        git_dir: PathBuf,
        /// The remote repository's common `$GIT_DIR` (object format source).
        common_git_dir: PathBuf,
    },
}

/// Whether push pack generation may use thin-pack deltas against remote objects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PushThinMode {
    /// Git's default: request/use thin packs unless the remote advertises
    /// `no-thin`.
    #[default]
    Auto,
    /// Explicit `--thin`; still respects a remote `no-thin` advertisement.
    Always,
    /// Explicit `--no-thin`.
    Never,
}

impl PushThinMode {
    pub(crate) fn wants_thin(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// Controls for a [`push`] run, mirroring the `git push` flags the CLI parses
/// that affect the wire/planning behavior the library owns.
///
/// `set-upstream` (`-u`) is intentionally absent: it only writes
/// `branch.<name>.remote`/`merge` config, which is a caller concern (the library
/// returns the executed commands in [`PushOutcome::commands`] so the caller can
/// drive that write). Atomic / push-options are likewise absent because the
/// CLI's HTTP and local push paths accept but do not act on them today.
#[derive(Debug, Clone, Copy, Default)]
pub struct PushOptions {
    /// Suppress the per-command side-effect of negotiating the `quiet`
    /// receive-pack capability (matching `git push --quiet`). Output suppression
    /// itself is a caller concern — the library always returns the outcome.
    pub quiet: bool,
    /// Force every update, bypassing the non-fast-forward check. Per-refspec `+`
    /// forces are honored independently of this flag.
    pub force: bool,
    /// Thin-pack behavior for transports that send a real receive-pack body.
    pub thin: PushThinMode,
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
    /// Bypass the non-fast-forward check for this command. This mirrors a
    /// refspec-local leading `+`; [`PushOptions::force`] still forces every
    /// command in the plan.
    pub force: bool,
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
                force: false,
            },
            PushAction::Update { dst, old, new } => Self {
                src: Some(new),
                dst,
                expected_old: Some(old),
                force: false,
            },
            PushAction::Delete { dst, old } => Self {
                src: None,
                dst,
                expected_old: old,
                force: false,
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

    pub fn from_commands(commands: Vec<PushCommand>, options: PushOptions) -> Self {
        Self {
            commands,
            pack_objects: Vec::new(),
            options,
        }
    }

    pub fn from_commands_and_infer_pack_roots(
        commands: Vec<PushCommand>,
        options: PushOptions,
    ) -> Self {
        let mut pack_objects = Vec::new();
        for command in &commands {
            let Some(src) = command.src.as_ref() else {
                continue;
            };
            if !pack_objects.contains(src) {
                pack_objects.push(*src);
            }
        }
        Self {
            commands,
            pack_objects,
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

/// Per-ref outcome of a push, mirroring git's `enum ref_status` so the CLI can
/// reproduce `transport_print_push_status` byte-for-byte. `Ok` covers create,
/// update, forced update, and delete (disambiguated by the old/new ids on the
/// owning [`PushReportRef`]); the remaining variants are the rejection reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRefStatus {
    /// The update was (or would be, under `--dry-run`) applied.
    Ok,
    /// The ref was already at the requested value; nothing to do.
    UpToDate,
    /// Local-side rejection: a non-forced non-fast-forward branch update.
    RejectNonFastForward,
    /// Local-side rejection: the remote tip is not present locally.
    RejectFetchFirst,
    /// `--force-with-lease`/`--force-if-includes` expectation was not met.
    RejectStale,
    /// `--force-if-includes`: tracking ref was updated but not integrated.
    RejectRemoteUpdated,
    /// Non-forced tag update where the remote tag already exists.
    RejectAlreadyExists,
    /// The receive-pack side reported `ng <ref> <message>`.
    RemoteReject(String),
    /// Part of an `--atomic` push that failed because a sibling ref was rejected.
    AtomicPushFailed,
}

/// One ref's line in git's push status report. Carries everything
/// `print_one_push_report` needs: the source ("from") ref, the destination
/// ("to") ref, the old/new object ids, whether the update was forced, whether it
/// is a deletion, and the classified [`PushRefStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReportRef {
    /// The local source ref name (git's `ref->peer_ref->name`), e.g.
    /// `refs/heads/main`. `None` for a deletion (git prints `:dst`).
    pub src: Option<String>,
    /// The destination ref name (git's `ref->name`), e.g. `refs/heads/main`.
    pub dst: String,
    /// The remote's old object id for `dst` (zero for a create).
    pub old_id: ObjectId,
    /// The object id installed at `dst` (zero for a delete).
    pub new_id: ObjectId,
    /// True when the update overwrote a non-fast-forward (git's `forced_update`).
    pub forced: bool,
    /// The classified outcome.
    pub status: PushRefStatus,
}

impl PushReportRef {
    /// Whether this ref is a deletion (new id is the zero oid).
    pub fn is_deletion(&self) -> bool {
        self.new_id.is_null()
    }

    /// Whether this ref's status counts as a push error (git's `push_had_errors`:
    /// anything that is not `Ok`/`UpToDate`/none).
    pub fn had_error(&self) -> bool {
        !matches!(self.status, PushRefStatus::Ok | PushRefStatus::UpToDate)
    }
}

/// The full result of a push as git's transport layer models it: every ref's
/// classified status, ready to be rendered into the "To <url>" report and used
/// to decide the process exit code and the `pull-before-push` advice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushStatusReport {
    /// Every requested ref, in planning order.
    pub refs: Vec<PushReportRef>,
}

impl PushStatusReport {
    /// True when any ref was rejected (git's overall push error flag).
    pub fn had_errors(&self) -> bool {
        self.refs.iter().any(PushReportRef::had_error)
    }

    /// True when at least one ref was actually updated (git's
    /// `transport_refs_pushed`): used to print "Everything up-to-date".
    pub fn refs_pushed(&self) -> bool {
        self.refs.iter().any(|reference| {
            reference.old_id != reference.new_id && matches!(reference.status, PushRefStatus::Ok)
        })
    }
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
        http_batch: crate::http::HttpOperationBatch,
        remote_url: RemoteUrl,
        features: ReceivePackFeatures,
        advertisements: Vec<RefAdvertisement>,
        pack_objects: Vec<ObjectId>,
    },
    Ssh(crate::ssh::SshPushPlan),
    Git(crate::git::GitPushPlan),
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
    // progress is not consumed yet. Kept named for the public API and future use.
    let _ = &mut services.progress;
    crate::protocol::check_transport_allowed(
        scheme_for_push_destination(request.destination),
        Some(request.config),
        None,
    )
    .map_err(crate::protocol::transport_policy_git_error)?;
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
        PushDestination::Git(remote_url) => {
            let plan = crate::git::plan_push_git(crate::git::GitPushRequest {
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
                PushExecution::Git(plan)
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
    let _ = &mut services.progress;
    crate::protocol::check_transport_allowed(
        scheme_for_push_destination(request.destination),
        Some(request.config),
        None,
    )
    .map_err(crate::protocol::transport_policy_git_error)?;
    let commands = receive_pack_commands_from_action_plan(request.format, request.plan)?;
    let command_forces = commands
        .iter()
        .cloned()
        .zip(request.plan.commands.iter())
        .map(|(command, planned)| (command, request.plan.options.force || planned.force))
        .collect::<Vec<_>>();
    match request.destination {
        #[cfg(feature = "http")]
        PushDestination::Http(remote_url) => {
            let http_batch = crate::http::HttpOperationBatch::new();
            let discovered = crate::http::http_service_advertisements(
                http_batch.client(),
                remote_url,
                request.format,
                GitService::ReceivePack,
                services.credentials,
            )?;
            let advertisement_set = discovered.set;
            let features = advertised_receive_pack_features(&advertisement_set.refs)?;
            verify_remote_object_format(&features, request.format)?;
            let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
            reject_non_fast_forward_pushes(
                request.common_git_dir,
                &local_db,
                request.format,
                &command_forces,
            )?;
            let execution = if commands.is_empty() {
                PushExecution::Noop
            } else {
                PushExecution::Http {
                    http_batch,
                    remote_url: remote_url.clone(),
                    features,
                    advertisements: advertisement_set.refs,
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
                command_forces: command_forces.clone(),
                pack_objects: request.plan.pack_objects.clone(),
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
        PushDestination::Git(remote_url) => {
            let plan = crate::git::plan_push_git_commands(crate::git::GitPushCommandsRequest {
                common_git_dir: request.common_git_dir,
                format: request.format,
                remote: remote_url,
                command_forces: command_forces.clone(),
                pack_objects: request.plan.pack_objects.clone(),
            })?;
            let commands = plan.commands.clone();
            let execution = if commands.is_empty() {
                PushExecution::Noop
            } else {
                PushExecution::Git(plan)
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
            reject_non_fast_forward_pushes(
                request.common_git_dir,
                &local_db,
                request.format,
                &command_forces,
            )?;
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

fn scheme_for_push_destination(destination: &PushDestination) -> &'static str {
    match destination {
        PushDestination::Http(remote) => crate::protocol::transport_scheme_for_remote(remote),
        PushDestination::Ssh(remote) => crate::protocol::transport_scheme_for_remote(remote),
        PushDestination::Git(remote) => crate::protocol::transport_scheme_for_remote(remote),
        PushDestination::Local { .. } => "file",
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
            http_batch,
            remote_url,
            features,
            advertisements,
            pack_objects,
        } => execute_push_http(
            request,
            services.credentials,
            http_batch,
            plan.commands,
            remote_url,
            features,
            advertisements,
            pack_objects,
        ),
        PushExecution::Ssh(plan) => crate::ssh::execute_push_ssh_plan(request, plan),
        PushExecution::Git(plan) => crate::git::execute_push_git_plan(request, plan),
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
    let http_batch = crate::http::HttpOperationBatch::new();
    let discovered = crate::http::http_service_advertisements(
        http_batch.client(),
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
    reject_non_fast_forward_pushes(common_git_dir, &local_db, format, &command_forces)?;
    let commands = commands_from_forces(&command_forces);
    let execution = if commands.is_empty() {
        PushExecution::Noop
    } else {
        PushExecution::Http {
            http_batch,
            remote_url: remote_url.clone(),
            features,
            advertisements: advertisement_set.refs,
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
    http_batch: crate::http::HttpOperationBatch,
    commands: Vec<ReceivePackCommand>,
    remote_url: RemoteUrl,
    features: ReceivePackFeatures,
    advertisements: Vec<RefAdvertisement>,
    pack_objects: Vec<ObjectId>,
) -> Result<PushOutcome> {
    let client = http_batch.client();
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let pack_request = PushPackRequest {
        local_db: &local_db,
        format: request.format,
        commands: &commands,
        pack_objects: &pack_objects,
        remote_advertisements: &advertisements,
        features: &features,
        options: receive_pack_push_options(&features, request.format, request.options.quiet),
        thin: request.options.thin.wants_thin(),
    };
    let url = http_smart_rpc_url(&remote_url, GitService::ReceivePack)?;
    let content_type = smart_http_rpc_request_content_type(GitService::ReceivePack)?;
    let post_buffer = http_post_buffer(request.config);
    let mut response = crate::http::http_send_with_auth(&remote_url, credentials, |auth| {
        let headers = crate::http::http_authorization_headers(auth);
        send_receive_pack_body(
            client,
            &url,
            &content_type,
            &headers,
            &pack_request,
            post_buffer,
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

/// git's `http.postBuffer` (default 1 MiB): a receive-pack request body that
/// fits within this many bytes is sent buffered with `Content-Length`; a larger
/// body is streamed with chunked transfer-encoding. Matching git here keeps the
/// common (small) push retry-safe under auth challenges while bounding memory
/// for large pushes.
#[cfg(feature = "http")]
fn http_post_buffer(config: &GitConfig) -> usize {
    const DEFAULT_POST_BUFFER: usize = 1 << 20;
    config
        .get("http", None, "postBuffer")
        .and_then(parse_post_buffer)
        .filter(|bytes| *bytes > 0)
        .unwrap_or(DEFAULT_POST_BUFFER)
}

/// Parse a git size value (`http.postBuffer`): a decimal byte count with an
/// optional `k`/`m`/`g` binary-unit suffix.
#[cfg(feature = "http")]
fn parse_post_buffer(raw: &str) -> Option<usize> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw.as_bytes().last() {
        Some(b'k' | b'K') => (&raw[..raw.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&raw[..raw.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        _ => (raw, 1),
    };
    digits
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
}

/// Send the receive-pack request body, choosing buffered (`Content-Length`) vs
/// streamed (chunked) delivery by `post_buffer`. The body is generated on a
/// scoped thread that pipes into the HTTP client, so a large pack is never held
/// fully in memory. A genuine generation failure is surfaced in preference to a
/// downstream transport error on a truncated body.
#[cfg(feature = "http")]
fn send_receive_pack_body(
    client: &dyn HttpClient,
    url: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    pack_request: &PushPackRequest<'_>,
    post_buffer: usize,
) -> Result<HttpResponse> {
    std::thread::scope(|scope| {
        let (mut reader, writer) = std::io::pipe().map_err(|err| GitError::Io(err.to_string()))?;
        let generator = scope.spawn(move || -> Result<()> {
            // `writer` is dropped at the end of this closure, signalling EOF to
            // the reader even on the error path.
            let mut writer = writer;
            write_receive_pack_body(pack_request, &mut writer)
        });

        // Probe up to `post_buffer + 1` bytes to decide buffered vs chunked
        // without first materialising the whole body.
        let mut probe = Vec::new();
        read_up_to(&mut reader, post_buffer.saturating_add(1), &mut probe)?;

        if probe.len() <= post_buffer {
            // Whole body fits the probe: the generator has reached EOF. Surface
            // any generation error before sending, then send with Content-Length
            // (re-runnable under auth retry).
            join_pack_generator(generator)?;
            client.post(url, content_type, headers, &probe)
        } else {
            // Large body: stream the probe followed by the rest of the pipe with
            // chunked encoding. Scope `body` so the pipe reader is dropped before
            // joining — a transport that stops early then unblocks the generator
            // via a broken pipe instead of deadlocking the join.
            let response = {
                let mut body = std::io::Cursor::new(probe).chain(reader);
                client.post_reader(url, content_type, headers, &mut body)
            };
            let generation = join_pack_generator(generator);
            match response {
                // An HTTP response (including 401) drives the caller's status and
                // auth-retry handling; the body was consumed, so prefer it.
                Ok(response) => Ok(response),
                Err(transport) => match generation {
                    Err(generation) => Err(generation),
                    Ok(()) => Err(transport),
                },
            }
        }
    })
}

/// Join the receive-pack body generator thread, flattening a panic into an I/O
/// error and propagating the generator's own `Result`.
#[cfg(feature = "http")]
fn join_pack_generator(handle: std::thread::ScopedJoinHandle<'_, Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(GitError::Io(
            "receive-pack body generator thread panicked".to_string(),
        )),
    }
}

/// Read from `reader` into `out` until `cap` bytes are buffered or EOF.
#[cfg(feature = "http")]
fn read_up_to(reader: &mut impl Read, cap: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut chunk = [0u8; 8192];
    while out.len() < cap {
        let want = (cap - out.len()).min(chunk.len());
        let read = reader
            .read(&mut chunk[..want])
            .map_err(|err| GitError::Io(err.to_string()))?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read]);
    }
    Ok(())
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
    reject_non_fast_forward_pushes(common_git_dir, &local_db, format, &command_forces)?;
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
    let remote_excluded_tips =
        remote_excluded_tip_roots(&remote_git_dir, request.format, &remote_refs)?;
    let starts = push_pack_roots(&commands, &pack_objects);
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, request.format);
    let remote_db = FileObjectDatabase::from_git_dir(&remote_common_git_dir, request.format);
    let remote_excluded =
        collect_reachable_object_ids(&remote_db, request.format, remote_excluded_tips)?;

    // git's `transfer.fsckObjects`: the receiving side fscks every object the
    // push introduces and rejects the push when one fails (most importantly a
    // malicious `.gitmodules` url). The local fast path copies objects directly
    // rather than through `index-pack --strict`, so run the same gate here.
    if remote_transfer_fsck_objects(&remote_common_git_dir) {
        fsck_pushed_objects(&local_db, request.format, &starts, &remote_excluded)?;
    }
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

/// Whether the local remote enables `transfer.fsckObjects` (the receiving-side
/// fsck gate). Reads only the remote's own config.
fn remote_transfer_fsck_objects(remote_common_git_dir: &Path) -> bool {
    GitConfig::read(remote_common_git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("transfer", None, "fsckObjects"))
        .unwrap_or(false)
}

/// Disposable object directory used to expose incoming local-push objects to
/// receive-side hooks without making them part of the destination repository.
pub struct PushQuarantine {
    object_dir: PathBuf,
}

impl PushQuarantine {
    pub fn object_dir(&self) -> &Path {
        &self.object_dir
    }
}

impl Drop for PushQuarantine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.object_dir);
    }
}

pub fn stage_local_push_quarantine(
    remote_git_dir: &Path,
    remote_common_git_dir: &Path,
    format: ObjectFormat,
    source_db: &FileObjectDatabase,
    commands: &[ReceivePackCommand],
) -> Result<Option<PushQuarantine>> {
    let starts = push_pack_roots(commands, &[]);
    if starts.is_empty() {
        return Ok(None);
    }
    let remote_refs = crate::local::local_fetch_advertisements(remote_git_dir, format)?;
    let remote_excluded_tips = remote_excluded_tip_roots(remote_git_dir, format, &remote_refs)?;
    let remote_db = FileObjectDatabase::from_git_dir(remote_common_git_dir, format);
    let remote_excluded = collect_reachable_object_ids(&remote_db, format, remote_excluded_tips)?;
    let object_dir = create_push_quarantine_object_dir(remote_common_git_dir)?;
    let quarantine_db = FileObjectDatabase::new(object_dir.clone(), format);
    let installed = match build_and_install_reachable_pack(
        source_db,
        &quarantine_db,
        format,
        starts,
        &remote_excluded,
        RawPackInstallOptions {
            promisor: false,
            ..Default::default()
        },
    ) {
        Ok(installed) => installed,
        Err(err) => {
            let _ = fs::remove_dir_all(&object_dir);
            return Err(err);
        }
    };
    if installed.is_none() {
        let _ = fs::remove_dir_all(&object_dir);
        return Ok(None);
    }
    Ok(Some(PushQuarantine { object_dir }))
}

fn create_push_quarantine_object_dir(remote_common_git_dir: &Path) -> Result<PathBuf> {
    let objects_dir = remote_common_git_dir.join("objects");
    fs::create_dir_all(&objects_dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100 {
        let object_dir = objects_dir.join(format!(
            "tmp_objdir-incoming-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&object_dir) {
            Ok(()) => {
                fs::create_dir_all(object_dir.join("pack"))?;
                fs::create_dir_all(object_dir.join("info"))?;
                return Ok(object_dir);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(GitError::Io(err.to_string())),
        }
    }
    Err(GitError::Io(
        "could not create push quarantine object directory".into(),
    ))
}

fn remote_excluded_tip_roots(
    remote_git_dir: &Path,
    format: ObjectFormat,
    remote_refs: &[RefAdvertisement],
) -> Result<Vec<ObjectId>> {
    let mut tips = remote_refs
        .iter()
        .map(|reference| reference.oid)
        .collect::<Vec<_>>();
    append_remote_alternate_ref_tips(remote_git_dir, format, &mut tips)?;
    Ok(tips)
}

fn append_remote_alternate_ref_tips(
    remote_git_dir: &Path,
    format: ObjectFormat,
    tips: &mut Vec<ObjectId>,
) -> Result<()> {
    let alternates = remote_git_dir.join("objects/info/alternates");
    let Ok(text) = fs::read_to_string(alternates) else {
        return Ok(());
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let objects_dir = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            remote_git_dir.join("objects").join(line)
        };
        let Some(alternate_git_dir) = objects_dir.parent() else {
            continue;
        };
        let store = FileRefStore::new(alternate_git_dir, format);
        let refs = match store.list_refs() {
            Ok(refs) => refs,
            Err(_) => continue,
        };
        for reference in refs {
            let Some((oid, _)) = resolve_for_each_ref_target(&store, &reference)? else {
                continue;
            };
            if !tips.contains(&oid) {
                tips.push(oid);
            }
        }
    }
    Ok(())
}

/// Run fsck over the objects this push introduces (reachable from `starts`,
/// minus what the remote already has). On any error-severity finding, print it
/// and reject the push — git's `transfer.fsckObjects` behavior.
fn fsck_pushed_objects(
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    starts: &[ObjectId],
    remote_excluded: &std::collections::HashSet<ObjectId>,
) -> Result<()> {
    if starts.is_empty() {
        return Ok(());
    }
    let new_objects: Vec<ObjectId> =
        collect_reachable_object_ids(local_db, format, starts.to_vec())?
            .into_iter()
            .filter(|oid| !remote_excluded.contains(oid))
            .collect();
    // The reader is the COMPLETE local db so link-walks never spuriously report
    // a "missing object" for something the remote already holds; only genuine
    // content errors (e.g. a disallowed .gitmodules url) in the new objects fail.
    let report = sley_fsck::fsck_objects(local_db, format, [], new_objects);
    if report.is_ok() {
        return Ok(());
    }
    for issue in &report.issues {
        if issue.severity == sley_fsck::IssueSeverity::Error {
            eprintln!("fatal: {}", issue.message);
        }
    }
    Err(GitError::Exit(128))
}

/// Fully resolved inputs for a status-reporting push to a local repository.
pub struct PushReportRequest<'a> {
    /// Local repository `$GIT_DIR`.
    pub git_dir: &'a Path,
    /// Local repository common `$GIT_DIR`, used for object access.
    pub common_git_dir: &'a Path,
    /// Local repository object format.
    pub format: ObjectFormat,
    /// The remote repository's `$GIT_DIR`.
    pub remote_git_dir: &'a Path,
    /// The remote repository's common `$GIT_DIR`.
    pub remote_common_git_dir: &'a Path,
    /// Refspecs requested by the caller (already URL/repo resolved).
    pub refspecs: &'a [String],
    /// Force every update (the `--force` flag).
    pub force: bool,
    /// `--atomic`: send nothing if any ref would be rejected.
    pub atomic: bool,
    /// `--dry-run`: classify and report, but do not send or update.
    pub dry_run: bool,
    /// Per-ref `--force-with-lease` expectations: `(dst, expected_old)`. An
    /// `expected_old` of `None` means "the remote ref must not exist".
    pub force_with_lease: &'a [(String, Option<ObjectId>)],
    /// `--force-with-lease` with no per-ref value: lease every pushed ref against
    /// its remote-tracking ref (git's implicit cas). The expected value per dst
    /// is supplied via [`Self::force_with_lease`]; this flag only governs whether
    /// a lease was requested at all (used for the "no actual ref" diagnostics).
    pub force_with_lease_default: bool,
    /// `--force-if-includes`: for tracking-based leases, reject when the current
    /// remote tip is not included in the local branch's reflog/history.
    pub force_if_includes: bool,
    /// Receive-pack-side config values supplied by the invoked receive-pack
    /// command, e.g. `--receive-pack="git -c receive.denyDeletes=false receive-pack"`.
    pub receive_config_overrides: &'a [(String, String)],
}

/// Push to a local repository, returning git's per-ref status report instead of
/// failing on the first rejection. Performs the client-side checks git's
/// send-pack does — non-fast-forward and `--force-with-lease` (stale info) — then
/// (unless `--dry-run`) sends the surviving commands and folds the receive-pack
/// report-status back into each ref. With `--atomic`, a single client-side
/// rejection turns every other ref into [`PushRefStatus::AtomicPushFailed`] and
/// nothing is sent. The caller renders the report and derives the exit code.
pub fn push_local_with_report(
    request: PushReportRequest<'_>,
    _config: &GitConfig,
) -> Result<PushStatusReport> {
    let format = request.format;
    let remote_format = crate::object_format_for_git_dir(request.remote_common_git_dir)?;
    if remote_format != format {
        return Err(GitError::InvalidObjectId(format!(
            "remote repository uses {}, local repository uses {}",
            remote_format.name(),
            format.name()
        )));
    }
    let local_store = FileRefStore::new(request.git_dir, format);
    let mut local_refs = local_push_source_refs(&local_store, format)?;
    add_revision_push_sources(request.git_dir, format, request.refspecs, &mut local_refs);
    let remote_refs = crate::local::local_fetch_advertisements(request.remote_git_dir, format)?;
    let planned = plan_push_command_sources(
        format,
        &local_refs,
        &remote_refs,
        request.refspecs,
        request.force,
    )?;
    let local_db = FileObjectDatabase::from_git_dir(request.common_git_dir, format);
    let remote_config =
        sley_config::read_repo_config(request.remote_git_dir, None).unwrap_or_default();

    // Classify each planned command the way git's send-pack does, collecting
    // rejections rather than bailing on the first one.
    let mut refs: Vec<PushReportRef> = Vec::new();
    for plan in &planned {
        let status = classify_push_command(
            &local_db,
            format,
            plan,
            &request,
            &remote_config,
            request.remote_git_dir,
        )?;
        // git's `forced_update` reflects either an actual rewind or a rejection
        // reason (e.g. stale lease) that was overridden by --force.
        let stale_lease_overridden = plan.force && lease_expectation_mismatch(&request, plan);
        let forced = matches!(status, PushRefStatus::Ok)
            && !plan.command.old_id.is_null()
            && !plan.command.new_id.is_null()
            && (stale_lease_overridden
                || if plan.command.name.starts_with("refs/heads/") {
                    !is_fast_forward(
                        request.common_git_dir,
                        &local_db,
                        format,
                        &plan.command.old_id,
                        &plan.command.new_id,
                    )?
                } else {
                    plan.force
                });
        refs.push(PushReportRef {
            src: plan.source.clone(),
            dst: plan.command.name.clone(),
            old_id: plan.command.old_id,
            new_id: plan.command.new_id,
            forced,
            status,
        });
    }

    let any_local_reject = refs.iter().any(|reference| {
        matches!(
            reference.status,
            PushRefStatus::RejectNonFastForward
                | PushRefStatus::RejectFetchFirst
                | PushRefStatus::RejectStale
                | PushRefStatus::RejectRemoteUpdated
                | PushRefStatus::RejectAlreadyExists
        )
    });

    // `--atomic`: if any ref was rejected client-side, send nothing and mark all
    // would-be-OK refs as atomic-push-failed (git's REF_STATUS_ATOMIC_PUSH_FAILED).
    // UpToDate refs are *not* converted — git leaves them reported as up to date.
    if request.atomic && any_local_reject {
        for reference in &mut refs {
            if matches!(reference.status, PushRefStatus::Ok) {
                reference.status = PushRefStatus::AtomicPushFailed;
            }
        }
        return Ok(PushStatusReport { refs });
    }

    if request.dry_run {
        return Ok(PushStatusReport { refs });
    }

    // Send only the commands that survived client-side checks.
    let send: Vec<ReceivePackCommand> = refs
        .iter()
        .filter(|reference| {
            matches!(reference.status, PushRefStatus::Ok) && reference.old_id != reference.new_id
        })
        .map(|reference| ReceivePackCommand {
            old_id: reference.old_id,
            new_id: reference.new_id,
            name: reference.dst.clone(),
        })
        .collect();

    if !send.is_empty() {
        let remote_excluded_tips =
            remote_excluded_tip_roots(request.remote_git_dir, format, &remote_refs)?;
        let pack_objects: Vec<ObjectId> = Vec::new();
        let starts = push_pack_roots(&send, &pack_objects);
        let remote_db = FileObjectDatabase::from_git_dir(request.remote_common_git_dir, format);
        let remote_excluded =
            collect_reachable_object_ids(&remote_db, format, remote_excluded_tips)?;
        // git's `transfer.fsckObjects`: fsck the introduced objects on the
        // receiving side and reject the push on a content error (a disallowed
        // `.gitmodules` url, a malformed object, ...).
        if remote_transfer_fsck_objects(request.remote_common_git_dir) {
            fsck_pushed_objects(&local_db, format, &starts, &remote_excluded)?;
        }
        let packfile = if starts.is_empty() {
            Vec::new()
        } else {
            b"PACK".to_vec()
        };
        let receive_request = ReceivePackPushRequest {
            commands: ReceivePackRequest {
                shallow: Vec::new(),
                commands: send.clone(),
                capabilities: Vec::new(),
            },
            push_options: None,
            packfile,
        };
        let report = crate::local::receive_pack_reachable_pack_into_local_repository(
            request.remote_git_dir,
            format,
            &receive_request,
            &local_db,
            starts,
            remote_excluded,
        )?;
        // Fold the receive-pack ng reports back onto the matching refs.
        if let ReceivePackUnpackStatus::Error(message) = &report.unpack {
            for reference in &mut refs {
                if matches!(reference.status, PushRefStatus::Ok) {
                    reference.status =
                        PushRefStatus::RemoteReject(format!("unpacker error: {message}"));
                }
            }
        }
        for command_status in &report.commands {
            if let ReceivePackCommandStatus::Ng { name, message } = command_status {
                for reference in &mut refs {
                    if reference.dst == *name && matches!(reference.status, PushRefStatus::Ok) {
                        reference.status = PushRefStatus::RemoteReject(message.clone());
                    }
                }
            }
        }
    }

    Ok(PushStatusReport { refs })
}

/// Classify one planned command into git's send-pack pre-flight status: an
/// up-to-date no-op, a non-fast-forward rejection, a `--force-with-lease` stale
/// rejection, or `Ok` (the command will be sent).
fn classify_push_command(
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    plan: &PlannedPushCommand,
    request: &PushReportRequest<'_>,
    config: &GitConfig,
    remote_git_dir: &Path,
) -> Result<PushRefStatus> {
    let command = &plan.command;

    if receive_ref_is_hidden(config, request.receive_config_overrides, &command.name) {
        let reason = if command.new_id.is_null() {
            "deny deleting a hidden ref"
        } else {
            "deny updating a hidden ref"
        };
        return Ok(PushRefStatus::RemoteReject(reason.to_string()));
    }

    // No change: the remote already has exactly this value (and it is not a
    // create-from-nothing of a non-existent ref). git reports UPTODATE.
    if command.old_id == command.new_id && !command.new_id.is_null() {
        return Ok(PushRefStatus::UpToDate);
    }

    if command.new_id.is_null() && !command.old_id.is_null() {
        if receive_config_bool(config, request.receive_config_overrides, "denydeletes")
            .unwrap_or(false)
        {
            return Ok(PushRefStatus::RemoteReject(
                "deletion prohibited".to_string(),
            ));
        }
        if receive_denies_current_branch_delete(format, command, config, request, remote_git_dir)? {
            return Ok(PushRefStatus::RemoteReject(
                "deletion of the current branch prohibited".to_string(),
            ));
        }
    }

    if command.name.starts_with("refs/heads/") && !command.new_id.is_null() {
        let object = local_db.read_object(&command.new_id)?;
        if object.object_type != ObjectType::Commit {
            return Ok(PushRefStatus::RemoteReject(
                "invalid new value provided".to_string(),
            ));
        }
    }

    // `--force-with-lease`: the remote's current value must match the lease, or
    // the push is rejected as stale info — checked before the non-ff gate and
    // independent of `--force`.
    if let Some((_, expected)) = request
        .force_with_lease
        .iter()
        .find(|(dst, _)| *dst == command.name)
    {
        let actual = if command.old_id.is_null() {
            None
        } else {
            Some(command.old_id)
        };
        if *expected != actual {
            if plan.force {
                return Ok(PushRefStatus::Ok);
            }
            return Ok(PushRefStatus::RejectStale);
        }
        if request.force_if_includes
            && !command.old_id.is_null()
            && (command.new_id.is_null()
                || !is_fast_forward(
                    request.common_git_dir,
                    local_db,
                    format,
                    &command.old_id,
                    &command.new_id,
                )?)
            && force_if_includes_rejects(
                request.common_git_dir,
                local_db,
                format,
                request.git_dir,
                &command.name,
                &command.old_id,
            )?
        {
            if plan.force {
                return Ok(PushRefStatus::Ok);
            }
            return Ok(PushRefStatus::RejectRemoteUpdated);
        }
        // A satisfied lease forces the update.
        return Ok(PushRefStatus::Ok);
    }

    if command.name.starts_with("refs/heads/")
        && !command.old_id.is_null()
        && !command.new_id.is_null()
        && !is_fast_forward(
            request.common_git_dir,
            local_db,
            format,
            &command.old_id,
            &command.new_id,
        )?
        && receive_config_bool(
            config,
            request.receive_config_overrides,
            "denynonfastforwards",
        )
        .unwrap_or(false)
    {
        return Ok(PushRefStatus::RemoteReject(format!(
            "denying non-fast-forward {} (you should pull first)",
            command.name
        )));
    }

    // Non-fast-forward branch update: rejected unless forced. Creations,
    // deletions, and non-branch refs skip this gate (matching git's send-pack).
    if !plan.force
        && command.name.starts_with("refs/tags/")
        && !command.old_id.is_null()
        && !command.new_id.is_null()
    {
        return Ok(PushRefStatus::RejectAlreadyExists);
    }

    if !plan.force
        && command.name.starts_with("refs/heads/")
        && !command.old_id.is_null()
        && !command.new_id.is_null()
    {
        if !local_db.contains(&command.old_id)? {
            return Ok(PushRefStatus::RejectFetchFirst);
        }
        if !is_fast_forward(
            request.common_git_dir,
            local_db,
            format,
            &command.old_id,
            &command.new_id,
        )? {
            return Ok(PushRefStatus::RejectNonFastForward);
        }
    }

    if !request.dry_run && receive_denies_current_branch(format, command, config, remote_git_dir)? {
        return Ok(PushRefStatus::RemoteReject(
            "branch is currently checked out".to_string(),
        ));
    }

    Ok(PushRefStatus::Ok)
}

fn receive_ref_is_hidden(
    config: &GitConfig,
    overrides: &[(String, String)],
    refname: &str,
) -> bool {
    let mut hide_refs = Vec::new();
    hide_refs.extend(hidden_ref_values(config, "transfer", None));
    hide_refs.extend(hidden_ref_values(config, "receive", None));
    hide_refs.extend(
        overrides
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("hiderefs"))
            .map(|(_, value)| trim_hidden_ref_pattern(value)),
    );
    ref_is_hidden_by_patterns(refname, &hide_refs)
}

fn hidden_ref_values(config: &GitConfig, section: &str, subsection: Option<&str>) -> Vec<String> {
    config
        .get_all(section, subsection, "hiderefs")
        .into_iter()
        .flatten()
        .map(trim_hidden_ref_pattern)
        .collect()
}

fn trim_hidden_ref_pattern(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

fn ref_is_hidden_by_patterns(refname: &str, patterns: &[String]) -> bool {
    for pattern in patterns.iter().rev() {
        let mut pattern = pattern.as_str();
        let negated = pattern.strip_prefix('!').is_some();
        if negated {
            pattern = &pattern[1..];
        }
        if let Some(rest) = pattern.strip_prefix('^') {
            pattern = rest;
        }
        if hidden_ref_pattern_matches(refname, pattern) {
            return !negated;
        }
    }
    false
}

fn hidden_ref_pattern_matches(refname: &str, pattern: &str) -> bool {
    refname
        .strip_prefix(pattern)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn lease_expectation_mismatch(request: &PushReportRequest<'_>, plan: &PlannedPushCommand) -> bool {
    let command = &plan.command;
    let actual = if command.old_id.is_null() {
        None
    } else {
        Some(command.old_id)
    };
    request
        .force_with_lease
        .iter()
        .find(|(dst, _)| *dst == command.name)
        .is_some_and(|(_, expected)| *expected != actual)
}

fn force_if_includes_rejects(
    common_git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    git_dir: &Path,
    local_ref: &str,
    remote_old: &ObjectId,
) -> Result<bool> {
    let store = FileRefStore::new(git_dir, format);
    let mut candidates = Vec::new();
    match store.read_ref(local_ref)? {
        Some(RefTarget::Direct(oid)) => candidates.push(oid),
        Some(RefTarget::Symbolic(target)) => {
            if let Some(RefTarget::Direct(oid)) = store.read_ref(&target)? {
                candidates.push(oid);
            }
        }
        None => return Ok(false),
    }
    for entry in store.read_reflog(local_ref)? {
        if !entry.new_oid.is_null() {
            candidates.push(entry.new_oid);
        }
    }
    candidates.sort();
    candidates.dedup();
    for candidate in candidates {
        if candidate == *remote_old {
            return Ok(false);
        }
        if let Ok(ancestors) = sley_rev::ancestor_depths(common_git_dir, format, db, &candidate)
            && ancestors.contains_key(remote_old)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn receive_config_bool(
    config: &GitConfig,
    overrides: &[(String, String)],
    key: &str,
) -> Option<bool> {
    overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
        .and_then(|(_, value)| sley_config::parse_config_bool(value))
        .or_else(|| config.get_bool("receive", None, key))
}

fn receive_denies_current_branch(
    format: ObjectFormat,
    command: &ReceivePackCommand,
    config: &GitConfig,
    remote_git_dir: &Path,
) -> Result<bool> {
    if command.new_id.is_null() {
        return Ok(false);
    }
    if !command.name.starts_with("refs/heads/") {
        return Ok(false);
    }
    let deny = config
        .get("receive", None, "denycurrentbranch")
        .unwrap_or("refuse");
    let denies = matches!(
        deny.to_ascii_lowercase().as_str(),
        "true" | "yes" | "on" | "1" | "refuse"
    );
    if !denies {
        return Ok(false);
    }
    if sley_worktree::worktree_root_for_git_dir(remote_git_dir)?.is_none() {
        return Ok(false);
    }
    let store = FileRefStore::new(remote_git_dir, format);
    Ok(matches!(
        store.read_ref("HEAD")?,
        Some(RefTarget::Symbolic(target)) if target == command.name
    ))
}

fn receive_targets_current_branch(
    format: ObjectFormat,
    command: &ReceivePackCommand,
    remote_git_dir: &Path,
) -> Result<bool> {
    if !command.name.starts_with("refs/heads/") {
        return Ok(false);
    }
    if sley_worktree::worktree_root_for_git_dir(remote_git_dir)?.is_none() {
        return Ok(false);
    }
    let store = FileRefStore::new(remote_git_dir, format);
    Ok(matches!(
        store.read_ref("HEAD")?,
        Some(RefTarget::Symbolic(target)) if target == command.name
    ))
}

fn receive_denies_current_branch_delete(
    format: ObjectFormat,
    command: &ReceivePackCommand,
    config: &GitConfig,
    request: &PushReportRequest<'_>,
    remote_git_dir: &Path,
) -> Result<bool> {
    if !receive_targets_current_branch(format, command, remote_git_dir)? {
        return Ok(false);
    }
    let deny = request
        .receive_config_overrides
        .iter()
        .rev()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case("denydeletecurrent"))
        .map(|(_, value)| value.as_str())
        .or_else(|| config.get("receive", None, "denydeletecurrent"))
        .unwrap_or("refuse");
    Ok(!matches!(
        deny.to_ascii_lowercase().as_str(),
        "ignore" | "warn" | "false" | "no" | "off" | "0"
    ))
}

/// Whether `old` is an ancestor of `new` (a fast-forward). A walk from `new`;
/// `old` reachable ⇒ fast-forward.
pub(crate) fn is_fast_forward(
    common_git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old: &ObjectId,
    new: &ObjectId,
) -> Result<bool> {
    let ancestors = sley_rev::ancestor_depths(common_git_dir, format, db, new)?;
    Ok(ancestors.contains_key(old))
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
pub(crate) fn plan_push_command_forces(
    format: ObjectFormat,
    local_refs: &[PushSourceRef],
    remote_refs: &[RefAdvertisement],
    refspecs: &[String],
    force: bool,
) -> Result<Vec<(ReceivePackCommand, bool)>> {
    let parsed_refspecs = refspecs
        .iter()
        .map(|refspec| {
            let normalized = normalize_push_refspec_for_sources(refspec, local_refs, remote_refs)?;
            parse_refspec(&normalized)
        })
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

/// One planned push command paired with its forcing flag and the local source
/// ref it came from (git's `ref->peer_ref`). A delete carries `source: None`.
struct PlannedPushCommand {
    command: ReceivePackCommand,
    force: bool,
    source: Option<String>,
}

/// Like [`plan_push_command_forces`], but also records the local source ref each
/// command resolved from so the status report can print the `from -> to` line.
/// The source is the normalized refspec source name; a delete (`:dst`) has no
/// source. A pattern refspec re-derives each expanded command's source from its
/// destination by reversing the wildcard substitution.
fn plan_push_command_sources(
    format: ObjectFormat,
    local_refs: &[PushSourceRef],
    remote_refs: &[RefAdvertisement],
    refspecs: &[String],
    force: bool,
) -> Result<Vec<PlannedPushCommand>> {
    let mut planned = Vec::new();
    for refspec in refspecs {
        let normalized = normalize_push_refspec_for_sources(refspec, local_refs, remote_refs)?;
        let parsed = parse_refspec(&normalized)?;
        let commands = plan_push_commands(
            format,
            local_refs,
            remote_refs,
            std::slice::from_ref(&parsed),
        )?;
        for command in commands {
            let source = push_command_source_name(&parsed, &command);
            planned.push(PlannedPushCommand {
                command,
                force: force || parsed.force,
                source,
            });
        }
    }
    Ok(planned)
}

/// Recover the local source ref name for one planned `command` from its owning
/// `refspec`. Deletes (no `src`) return `None`. A wildcard pattern reverses the
/// substitution: the command's destination minus the pattern's destination
/// affix yields the matched stem, which slots into the pattern's source affix.
fn push_command_source_name(refspec: &RefSpec, command: &ReceivePackCommand) -> Option<String> {
    let src = refspec.src.as_deref()?;
    if !refspec.pattern {
        return Some(src.to_string());
    }
    let (src_prefix, src_suffix) = src.split_once('*')?;
    let dst = refspec.dst.as_deref()?;
    let (dst_prefix, dst_suffix) = dst.split_once('*')?;
    let stem = command
        .name
        .strip_prefix(dst_prefix)
        .and_then(|rest| rest.strip_suffix(dst_suffix))?;
    Some(format!("{src_prefix}{stem}{src_suffix}"))
}

pub(crate) fn add_revision_push_sources(
    git_dir: &Path,
    format: ObjectFormat,
    refspecs: &[String],
    local_refs: &mut Vec<PushSourceRef>,
) {
    for refspec in refspecs {
        let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
        let src = refspec.split_once(':').map_or(refspec, |(src, _)| src);
        if src.is_empty() || src == "HEAD" {
            continue;
        }
        if src.starts_with("refs/") && local_refs.iter().any(|reference| reference.name == src) {
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

fn normalize_push_refspec_for_sources(
    refspec: &str,
    local_refs: &[PushSourceRef],
    remote_refs: &[RefAdvertisement],
) -> Result<String> {
    let (force, refspec) = refspec
        .strip_prefix('+')
        .map_or((false, refspec), |refspec| (true, refspec));
    let normalized = if let Some((src, dst)) = refspec.split_once(':') {
        let (src, src_kind) = normalize_push_source_refname(src, local_refs);
        let dst = if src.is_empty() {
            normalize_push_delete_destination_refname(dst, remote_refs)?
        } else {
            normalize_push_destination_refname(dst, src_kind, remote_refs)?
        };
        if !src.is_empty() && !dst.contains('*') && push_destination_is_onelevel_under_refs(&dst) {
            return Err(GitError::Command(format!(
                "destination refspec {dst} is not a valid ref"
            )));
        }
        format!("{src}:{dst}")
    } else {
        let (name, _) = normalize_push_source_refname(refspec, local_refs);
        // A colon-less refspec re-uses the source's *resolved* full name as the
        // implicit destination (git's `match_explicit`: a NULL dst resolves to
        // the matched source ref). That full name is then disambiguated against
        // the remote's existing refs, so `git push <remote> frotz` (a tag)
        // lands on `refs/tags/frotz` even when the remote also has a same-named
        // branch.
        let dst = match count_refspec_match_dst(&name, remote_refs) {
            DstMatch::Unique(matched) => matched.to_string(),
            DstMatch::None => name.clone(),
            DstMatch::Ambiguous => {
                return Err(GitError::Command(format!(
                    "dst refspec {name} matches more than one"
                )));
            }
        };
        format!("{name}:{dst}")
    };
    Ok(if force {
        format!("+{normalized}")
    } else {
        normalized
    })
}

/// git's `refname_match`: true when `full_name` equals `abbrev` expanded by one
/// of the `ref_rev_parse_rules`. Returns the matched rule's rank (higher = more
/// specific) so the caller can replicate git's strong/weak distinction.
fn refname_match_rank(abbrev: &str, full_name: &str) -> Option<usize> {
    const RULES: [&str; 6] = [
        "{}",
        "refs/{}",
        "refs/tags/{}",
        "refs/heads/{}",
        "refs/remotes/{}",
        "refs/remotes/{}/HEAD",
    ];
    for (idx, rule) in RULES.iter().enumerate() {
        let (prefix, suffix) = rule.split_once("{}").unwrap_or((rule, ""));
        if full_name == format!("{prefix}{abbrev}{suffix}") {
            return Some(RULES.len() - idx);
        }
    }
    None
}

/// The outcome of git's `count_refspec_match` for a push destination.
enum DstMatch<'a> {
    /// Exactly one acceptable match (one strong, or zero strong + one weak).
    Unique(&'a str),
    /// No remote ref matched — the caller should `guess_ref` or use the literal.
    None,
    /// More than one match — git dies with "dst refspec … matches more than one".
    Ambiguous,
}

/// git's `count_refspec_match` for a push destination: find the unique existing
/// remote ref that `pattern` resolves to, distinguishing strong matches (full
/// name, top-level, or a head/tag) from weak ones (a partial match outside
/// heads/tags, e.g. `origin/main` → `refs/remotes/origin/main`). One strong
/// match wins outright; with no strong match a single weak match is used; more
/// than one acceptable match is ambiguous.
fn count_refspec_match_dst<'a>(pattern: &str, remote_refs: &'a [RefAdvertisement]) -> DstMatch<'a> {
    let patlen = pattern.len();
    let mut strong: Option<&str> = None;
    let mut strong_count = 0usize;
    let mut weak: Option<&str> = None;
    let mut weak_count = 0usize;
    for advert in remote_refs {
        let name = advert.name.as_str();
        if refname_match_rank(pattern, name).is_none() {
            continue;
        }
        let namelen = name.len();
        let is_weak = namelen != patlen
            && patlen + 5 != namelen
            && !name.starts_with("refs/heads/")
            && !name.starts_with("refs/tags/");
        if is_weak {
            weak = Some(name);
            weak_count += 1;
        } else {
            strong = Some(name);
            strong_count += 1;
        }
    }
    match (strong_count, weak_count, strong, weak) {
        (1, _, Some(matched), _) => DstMatch::Unique(matched),
        (0, 1, _, Some(matched)) => DstMatch::Unique(matched),
        (0, 0, _, _) => DstMatch::None,
        _ => DstMatch::Ambiguous,
    }
}

#[derive(Clone, Copy)]
enum PushSourceKind {
    Branch,
    Tag,
    /// A source ref that resolves but is neither under `refs/heads/` nor
    /// `refs/tags/` (e.g. `HEAD`, a fully-qualified `refs/...` name). git's
    /// `guess_ref` still guesses `refs/heads/<dst>` for these.
    Other,
    /// A source that is NOT a ref at all (a raw object id or a rev-expression
    /// like `main^`). git's `guess_ref` resolves nothing for these, so an
    /// unqualified destination cannot be guessed and the push is rejected.
    Unqualifiable,
}

fn normalize_push_source_refname(
    name: &str,
    local_refs: &[PushSourceRef],
) -> (String, PushSourceKind) {
    // `@` is git's documented alias for `HEAD`; like `HEAD` it resolves to a
    // branch, so `guess_ref` can still qualify an unqualified destination.
    if name.is_empty() || name == "HEAD" || name == "@" || name.starts_with("refs/") {
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
        // A literal match outside heads/tags/HEAD/refs is a revision source
        // injected by `add_revision_push_sources` (an oid or `main^`-style
        // expression) — not a ref, so a partial dst cannot be guessed.
        (name.to_string(), PushSourceKind::Unqualifiable)
    } else {
        (branch, PushSourceKind::Branch)
    }
}

fn normalize_push_delete_destination_refname(
    name: &str,
    remote_refs: &[RefAdvertisement],
) -> Result<String> {
    if name.is_empty() || name == "HEAD" || name.starts_with("refs/") {
        return Ok(name.to_string());
    }
    match count_refspec_match_dst(name, remote_refs) {
        DstMatch::Unique(matched) => Ok(matched.to_string()),
        DstMatch::Ambiguous => Err(GitError::Command(format!(
            "dst refspec {name} matches more than one"
        ))),
        DstMatch::None => Err(GitError::reference_not_found(format!("remote ref {name}"))),
    }
}

fn normalize_push_destination_refname(
    name: &str,
    src_kind: PushSourceKind,
    remote_refs: &[RefAdvertisement],
) -> Result<String> {
    if name.is_empty() || name == "HEAD" || name.starts_with("refs/") {
        return Ok(name.to_string());
    }
    // git's `match_explicit`: a partial destination first resolves against the
    // remote's existing refs (so `main:origin/main` lands on the existing
    // `refs/remotes/origin/main`); an ambiguous match is fatal; only when
    // nothing matches does it fall back to `guess_ref`'s heads/tags choice
    // driven by the source ref's kind.
    match count_refspec_match_dst(name, remote_refs) {
        DstMatch::Unique(matched) => Ok(matched.to_string()),
        DstMatch::Ambiguous => Err(GitError::Command(format!(
            "dst refspec {name} matches more than one"
        ))),
        DstMatch::None => match src_kind {
            PushSourceKind::Tag => Ok(format!("refs/tags/{name}")),
            PushSourceKind::Branch | PushSourceKind::Other => Ok(format!("refs/heads/{name}")),
            // git's `guess_ref` returns NULL for a non-ref source, so the
            // unqualified destination is unresolvable (the "destination is not a
            // full refname … you must fully qualify the ref" error).
            PushSourceKind::Unqualifiable => Err(GitError::Command(format!(
                "the destination you provided is not a full refname (i.e., starting with \"refs/\"); unable to guess the destination for {name}"
            ))),
        },
    }
}

fn push_destination_is_onelevel_under_refs(name: &str) -> bool {
    name.strip_prefix("refs/")
        .is_some_and(|rest| !rest.contains('/'))
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

/// Redact embedded credentials from a push URL before showing it in
/// user-visible diagnostics.
pub fn push_url_for_display(url: &str) -> String {
    redact_url_for_display(url)
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
    common_git_dir: &Path,
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
        let ancestors = sley_rev::ancestor_depths(common_git_dir, format, local_db, &command.new_id)?;
        if !ancestors.contains_key(&command.old_id) {
            let short = command.name.trim_start_matches("refs/heads/");
            return Err(GitError::Command(format!(
                "failed to push some refs: non-fast-forward update to {short}"
            )));
        }
    }
    Ok(())
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
            thin: PushThinMode::Auto,
        }
    }

    /// Records the last send and which `HttpClient` method delivered it, so the
    /// streaming-vs-buffered gate can be asserted along with byte-for-byte body
    /// equality across both paths.
    #[derive(Default)]
    struct RecordingClient {
        last: std::sync::Mutex<Option<(&'static str, Vec<u8>)>>,
    }

    impl RecordingClient {
        fn take(&self) -> (&'static str, Vec<u8>) {
            self.last
                .lock()
                .expect("lock")
                .take()
                .expect("a send was recorded")
        }

        fn ok_response() -> Result<HttpResponse> {
            Ok(HttpResponse {
                status: 200,
                content_type: None,
                body: Box::new(std::io::empty()),
            })
        }
    }

    impl HttpClient for RecordingClient {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
            Self::ok_response()
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<HttpResponse> {
            *self.last.lock().expect("lock") = Some(("post", body.to_vec()));
            Self::ok_response()
        }

        fn post_reader(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            body: &mut dyn Read,
        ) -> Result<HttpResponse> {
            let mut buffered = Vec::new();
            body.read_to_end(&mut buffered)
                .map_err(|err| GitError::Io(err.to_string()))?;
            *self.last.lock().expect("lock") = Some(("post_reader", buffered));
            Self::ok_response()
        }
    }

    fn receive_pack_request<'a>(
        db: &'a FileObjectDatabase,
        commands: &'a [ReceivePackCommand],
        advertisements: &'a [RefAdvertisement],
        features: &'a ReceivePackFeatures,
    ) -> PushPackRequest<'a> {
        PushPackRequest {
            local_db: db,
            format: ObjectFormat::Sha1,
            commands,
            pack_objects: &[],
            remote_advertisements: advertisements,
            features,
            options: ReceivePackPushRequestOptions {
                report_status: true,
                ofs_delta: true,
                ..ReceivePackPushRequestOptions::default()
            },
            thin: false,
        }
    }

    #[test]
    fn send_receive_pack_body_gates_on_post_buffer_and_preserves_bytes() {
        let git_dir = temp_repo("send-receive-pack-gate");
        let commit = write_commit(&git_dir, vec![], "streamed http push");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let commands = [ReceivePackCommand {
            old_id: ObjectId::null(ObjectFormat::Sha1),
            new_id: commit,
            name: "refs/heads/main".into(),
        }];
        let features = ReceivePackFeatures {
            report_status: true,
            ofs_delta: true,
            ..ReceivePackFeatures::default()
        };
        let req = receive_pack_request(&db, &commands, &[], &features);

        // The canonical body the streaming and buffered paths must both deliver.
        let mut canonical = Vec::new();
        write_receive_pack_body(&req, &mut canonical).expect("canonical body");
        assert!(canonical.len() > 1, "body should be non-trivial");

        // A post_buffer larger than the body → buffered Content-Length send.
        let buffered_client = RecordingClient::default();
        send_receive_pack_body(
            &buffered_client,
            "http://h/git-receive-pack",
            "ct",
            &[],
            &req,
            usize::MAX,
        )
        .expect("buffered send");
        let (method, body) = buffered_client.take();
        assert_eq!(method, "post");
        assert_eq!(body, canonical);

        // A post_buffer smaller than the body → streamed chunked send. The probe
        // (post_buffer + 1 bytes) plus the rest of the pipe must reproduce the
        // exact same bytes.
        let streamed_client = RecordingClient::default();
        send_receive_pack_body(
            &streamed_client,
            "http://h/git-receive-pack",
            "ct",
            &[],
            &req,
            8,
        )
        .expect("streamed send");
        let (method, body) = streamed_client.take();
        assert_eq!(method, "post_reader");
        assert_eq!(body, canonical);

        let _ = fs::remove_dir_all(git_dir.parent().unwrap_or(&git_dir));
    }

    #[test]
    fn parse_post_buffer_reads_git_size_values() {
        assert_eq!(parse_post_buffer("1048576"), Some(1 << 20));
        assert_eq!(parse_post_buffer("512k"), Some(512 * 1024));
        assert_eq!(parse_post_buffer("1M"), Some(1024 * 1024));
        assert_eq!(parse_post_buffer("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_post_buffer("  64k "), Some(64 * 1024));
        assert_eq!(parse_post_buffer("garbage"), None);
        assert_eq!(parse_post_buffer(""), None);
    }

    #[test]
    fn push_action_plan_infers_pack_roots_from_non_delete_commands() {
        let repo = temp_repo("action-plan-infer-roots");
        let first = write_commit(&repo, Vec::new(), "first");
        let second = write_commit(&repo, vec![first], "second");

        let plan = PushActionPlan::from_commands_and_infer_pack_roots(
            vec![
                PushCommand {
                    src: Some(first),
                    dst: "refs/heads/main".into(),
                    expected_old: None,
                    force: false,
                },
                PushCommand {
                    src: Some(second),
                    dst: "refs/heads/topic".into(),
                    expected_old: Some(first),
                    force: true,
                },
            ],
            default_options(),
        );

        assert_eq!(plan.pack_objects, vec![first, second]);
        assert!(!plan.commands[0].force);
        assert!(plan.commands[1].force);
    }

    #[test]
    fn push_action_plan_inferred_pack_roots_exclude_deletes() {
        let repo = temp_repo("action-plan-delete-roots");
        let old = write_commit(&repo, Vec::new(), "old");
        let new = write_commit(&repo, vec![old], "new");

        let plan = PushActionPlan::from_commands_and_infer_pack_roots(
            vec![
                PushCommand {
                    src: None,
                    dst: "refs/heads/remove".into(),
                    expected_old: Some(old),
                    force: false,
                },
                PushCommand {
                    src: Some(new),
                    dst: "refs/heads/keep".into(),
                    expected_old: Some(old),
                    force: false,
                },
            ],
            default_options(),
        );

        assert_eq!(plan.pack_objects, vec![new]);
    }

    #[test]
    fn push_action_plan_inferred_pack_roots_dedupe_first_seen_order() {
        let repo = temp_repo("action-plan-dedupe-roots");
        let first = write_commit(&repo, Vec::new(), "first");
        let second = write_commit(&repo, Vec::new(), "second");

        let plan = PushActionPlan::from_commands_and_infer_pack_roots(
            vec![
                PushCommand {
                    src: Some(second),
                    dst: "refs/heads/second".into(),
                    expected_old: None,
                    force: false,
                },
                PushCommand {
                    src: Some(first),
                    dst: "refs/heads/first".into(),
                    expected_old: None,
                    force: false,
                },
                PushCommand {
                    src: Some(second),
                    dst: "refs/tags/second".into(),
                    expected_old: None,
                    force: false,
                },
                PushCommand {
                    src: Some(first),
                    dst: "refs/tags/first".into(),
                    expected_old: None,
                    force: false,
                },
            ],
            default_options(),
        );

        assert_eq!(plan.pack_objects, vec![second, first]);
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
    fn local_push_actions_honors_per_command_force() {
        let local = temp_repo("actions-command-force-local");
        let remote = temp_repo("actions-command-force-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let unrelated = write_commit(&local, Vec::new(), "unrelated");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(base));

        let unforced = PushActionPlan::from_commands(
            vec![PushCommand {
                src: Some(unrelated),
                dst: "refs/heads/main".into(),
                expected_old: Some(base),
                force: false,
            }],
            default_options(),
        );
        let err = push_local_actions(&local, &remote, &unforced)
            .expect_err("non-fast-forward should reject without command force");
        assert!(err.to_string().contains("non-fast-forward"));

        let forced = PushActionPlan::from_commands(
            vec![PushCommand {
                src: Some(unrelated),
                dst: "refs/heads/main".into(),
                expected_old: Some(base),
                force: true,
            }],
            default_options(),
        );
        let outcome = push_local_actions(&local, &remote, &forced).expect("command force pushes");

        assert_eq!(outcome.commands.len(), 1);
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(unrelated))
        );
    }

    #[test]
    fn local_push_actions_command_force_is_precise_for_non_ff_validation() {
        let local = temp_repo("actions-command-force-precise-local");
        let remote = temp_repo("actions-command-force-precise-remote");
        let base = write_commit(&local, Vec::new(), "base");
        let remote_base = write_commit(&remote, Vec::new(), "base");
        assert_eq!(remote_base, base);
        let forced_unrelated = write_commit(&local, Vec::new(), "forced unrelated");
        let unforced_unrelated = write_commit(&local, Vec::new(), "unforced unrelated");
        set_ref(&remote, "refs/heads/main", RefTarget::Direct(base));
        set_ref(&remote, "refs/heads/topic", RefTarget::Direct(base));
        let plan = PushActionPlan::from_commands_and_infer_pack_roots(
            vec![
                PushCommand {
                    src: Some(forced_unrelated),
                    dst: "refs/heads/main".into(),
                    expected_old: Some(base),
                    force: true,
                },
                PushCommand {
                    src: Some(unforced_unrelated),
                    dst: "refs/heads/topic".into(),
                    expected_old: Some(base),
                    force: false,
                },
            ],
            default_options(),
        );

        let err = push_local_actions(&local, &remote, &plan)
            .expect_err("only the forced command should bypass non-fast-forward validation");

        assert!(err.to_string().contains("non-fast-forward update to topic"));
        let remote_refs = FileRefStore::new(&remote, ObjectFormat::Sha1);
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/main")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(base))
        );
        assert_eq!(
            remote_refs
                .read_ref("refs/heads/topic")
                .expect("remote ref should read"),
            Some(RefTarget::Direct(base))
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
    fn push_url_for_display_redacts_embedded_credentials() {
        assert_eq!(
            push_url_for_display("https://user:pass@host/repo.git"),
            "https://<redacted>@host/repo.git"
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
