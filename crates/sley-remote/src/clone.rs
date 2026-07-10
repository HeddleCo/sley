//! Callable clone orchestration for HTTP(S) and local (`file://`/path) remotes.
//!
//! [`clone`] performs the transport-shaped core of `git clone` for the common
//! branch-tracking case: it initializes the destination repository, fetches from
//! the resolved remote (reusing the Stage E [`crate::fetch`] machinery), creates
//! the local branch at the fetched remote tip, points `refs/remotes/<origin>/HEAD`
//! at the remote default branch, and checks out the worktree (via
//! [`sley_worktree`]). Everything is taken as explicit parameters — the
//! destination, the [`ObjectFormat`], the resolved [`CloneSource`], a
//! [`CloneOptions`], two caller callbacks, and the seam objects
//! ([`CredentialProvider`], [`ProgressSink`]) — so it never reads process-global
//! state, mutates the process CWD, parses arguments, or prints.
//!
//! Crucially, [`clone`] takes the destination `git_dir` implicitly (from the
//! init it performs) and drives the fetch against it directly, so there is no
//! `set_current_dir` dance: the CLI's old clone path chdir'd into the new repo so
//! its `discover_git_dir`/`ls_remote_resolved_url` helpers would resolve the
//! freshly-created repository, then restored the CWD. Here the repository and
//! remote are already resolved by the caller and passed in, so the process CWD is
//! never touched.
//!
//! The CLI keeps everything that is policy or presentation: argument parsing, the
//! "Cloning into…"/"done." lines and `--depth`/`--filter` warnings, the
//! unsupported-option gating (bare/mirror, `--revision`, `--shared`/`--reference`,
//! `--bundle-uri`, SHA-256 over HTTP), and the post-checkout steps
//! (`--no-checkout` worktree removal, `--sparse`, `--separate-git-dir`). The two
//! `configure` callbacks let the CLI run its own config-writing helpers (template
//! application, `remote.<origin>.*`, `-c` overrides, `submodule.active`, branch
//! upstream) at the right points in the flow while returning the [`GitConfig`]
//! the next step needs, keeping that CLI-coupled config I/O out of the library.
//!
//! SSH clone uses the same [`crate::fetch`] SSH dispatch as fetch; only the
//! caller-side URL resolution and post-clone presentation stay in the CLI.

use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::{InitOptions, RefStorageFormat, RepositoryBootstrap};
use sley_object::{Commit, ObjectType, Tree};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_refs::{FileRefStore, RefTarget, RefUpdate};
use sley_transport::RemoteUrl;

use crate::fetch::{FetchOptions, FetchSource, fetch};
use crate::{CredentialProvider, ProgressSink};

/// Internal placeholder branch used while clone initializes before it knows
/// which branch, detached commit, or unborn remote state will own `HEAD`.
const CLONE_UNBORN_BRANCH: &str = "__sley_clone_unborn__";

/// How [`clone`] reaches the remote it is cloning from.
///
/// The caller resolves the remote (URL rewriting, repository discovery — all
/// process-state dependent) and hands `clone` a concrete transport.
pub enum CloneSource {
    /// A smart-HTTP(S) remote at the given already-resolved URL.
    Http(RemoteUrl),
    /// An SSH remote at the given already-resolved URL. Fetched by spawning `ssh`
    /// (the credential seam is unused — the `ssh` program owns authentication).
    Ssh(RemoteUrl),
    /// A native anonymous `git://` remote at the given already-resolved URL.
    Git {
        remote: RemoteUrl,
        protocol_v2: bool,
    },
    /// A local repository served in-process from `git_dir`.
    Local {
        /// The remote repository's `$GIT_DIR`.
        git_dir: PathBuf,
        /// The remote repository's common `$GIT_DIR` (object format source).
        common_git_dir: PathBuf,
    },
}

/// The clone inputs the library needs for the branch-tracking flow, all resolved
/// by the caller. The remaining `git clone` knobs (bare/mirror, `--revision`,
/// templates, config overrides, sparse, separate-git-dir, etc.) stay in the CLI:
/// the unsupported ones are gated before `clone` is called, and the config-writing
/// ones run inside the `configure`/`configure_branch` callbacks.
pub struct CloneOptions<'a> {
    /// The remote name to configure and track (`--origin`, default `origin`).
    pub origin: &'a str,
    /// The branch to create locally and check out (the requested `--branch` or
    /// the remote's default branch).
    pub checkout_branch: &'a str,
    /// The remote's default branch, used to decide whether to point
    /// `refs/remotes/<origin>/HEAD` at it.
    pub remote_head_branch: &'a str,
    /// Whether only `checkout_branch` was fetched (`--single-branch`); when set,
    /// `refs/remotes/<origin>/HEAD` is only written if the checked-out branch is
    /// the remote default.
    pub single_branch: bool,
    /// Shallow clone depth (`--depth N`): truncate history to `N` commits per tip,
    /// writing `$GIT_DIR/shallow`. `None` is a full clone. Honored by the HTTP
    /// and SSH transports and by the in-process local server (`git clone
    /// --no-local --depth N <path>`); a depth on a plain local clone is
    /// warned-and-ignored upstream of `clone` by the caller, matching git's
    /// `is_local` behavior.
    pub depth: Option<u32>,
    /// `--shallow-since=<date>` (parsed to an epoch): deepen to commits newer
    /// than the date. Local in-process transport only.
    pub deepen_since: Option<i64>,
    /// `--shallow-exclude=<ref>` values, resolved against the remote.
    pub deepen_not: Vec<String>,
    /// The committer identity for the branch-creation and checkout reflog entries.
    pub committer: Vec<u8>,
    /// The remote `HEAD` is detached at this commit (no default branch). After
    /// the fetch the destination checks out this commit detached instead of
    /// creating `checkout_branch`; `refs/remotes/<origin>/HEAD` is not written.
    pub detached_head: Option<ObjectId>,
    /// Whether clone should populate the worktree. `--no-checkout` still writes
    /// refs/config but must not hydrate filtered blobs solely for checkout.
    pub checkout: bool,
    /// Partial-clone object filter (`--filter=blob:none`) to apply to the
    /// clone fetch. Only honored by the in-process local server.
    pub filter: Option<sley_odb::PackObjectFilter>,
    /// Whether `checkout_branch` came from an explicit `--branch`. When set, a
    /// missing remote tip for that branch is a hard error ("Remote branch … not
    /// found"); when unset, a missing tip is an empty/unborn-repository clone.
    pub branch_explicit: bool,
    /// Destination repository ref storage format.
    pub ref_storage: RefStorageFormat,
    /// SSH command-line shape for the clone's internal fetch, used for
    /// clone-only flags like `-4`/`-6`.
    pub ssh_options: Option<crate::ssh::SshTransportOptions>,
    /// Explicit SSH upload-pack program selected by clone's `--upload-pack`.
    pub upload_pack_command: Option<&'a str>,
    /// Refuse cloning from a shallow source (`--reject-shallow`).
    pub reject_shallow: bool,
}

/// The structured result of a [`clone`].
#[derive(Debug, Clone)]
pub struct CloneOutcome {
    /// The destination repository's `$GIT_DIR` (the `.git` directory created by
    /// the init step). The caller uses it for its post-checkout steps.
    pub git_dir: PathBuf,
    /// The object id the local branch was created at (the fetched remote tip),
    /// or `None` when the remote was empty/unborn (no branch was created and
    /// `HEAD` was left as an unborn symref to `checkout_branch`).
    pub branch_oid: Option<ObjectId>,
    /// True when the remote advertised no refs for `checkout_branch` and no
    /// `--branch`/`--revision` was requested: an empty/unborn-repository clone.
    /// The caller prints git's "You appear to have cloned an empty repository."
    /// warning and skips the worktree checkout.
    pub empty: bool,
}

/// Fully resolved inputs for a [`clone`] run.
pub struct CloneRequest<'a> {
    /// Destination worktree/repository path.
    pub destination: &'a Path,
    /// Explicit destination git directory, used by `GIT_WORK_TREE git clone`
    /// where the command-line directory is the repository admin dir and the
    /// worktree lives elsewhere.
    pub git_dir_override: Option<&'a Path>,
    /// Value to write as `core.worktree` when `git_dir_override` separates the
    /// admin dir from the checkout root.
    pub core_worktree: Option<&'a str>,
    /// Destination repository object format.
    pub format: ObjectFormat,
    /// Already-resolved clone source.
    pub source: &'a CloneSource,
    /// Clone behavior and branch-tracking options.
    pub options: &'a CloneOptions<'a>,
}

/// Mutable seams used while cloning.
pub struct CloneServices<'a> {
    /// Callback that writes initial repository config and returns the resulting
    /// config snapshot used for the fetch.
    pub configure: &'a mut dyn FnMut(&Path) -> Result<GitConfig>,
    /// Callback that writes local branch upstream config and returns the config
    /// snapshot used for checkout filtering.
    pub configure_branch: &'a mut dyn FnMut(&Path, &str) -> Result<GitConfig>,
    /// Credential source for authenticated transports.
    pub credentials: &'a mut dyn CredentialProvider,
    /// Progress sink for fetch progress/prune notices.
    pub progress: &'a mut dyn ProgressSink,
}

/// Clone the resolved `source` into a fresh repository at `destination`.
///
/// Performs the transport-shaped core the CLI's `clone_http_repository` and the
/// inline local clone path shared: initializes the repository, invokes
/// `configure` to let the caller write the new repo's config (returning the
/// [`GitConfig`] to fetch against), fetches the configured refs (reusing
/// [`crate::fetch::fetch`] with clone's fixed options), creates the local
/// `checkout_branch` at its fetched remote tip, invokes `configure_branch` to let
/// the caller write the branch's upstream config (returning the [`GitConfig`] to
/// check out against), points `refs/remotes/<origin>/HEAD` at the remote default
/// branch when appropriate, and checks out the worktree.
///
/// `configure` runs right after init (before the fetch) and must return the
/// repository config; `configure_branch` runs right after the local branch is
/// created (before the worktree checkout) and must return the config used for
/// checkout. Splitting the config writes into these callbacks keeps the CLI's
/// config I/O helpers (which depend on CLI-specific config serialization and
/// templates) out of the library while preserving their ordering in the flow.
///
/// Emits any library-side progress through `progress` and returns the structured
/// [`CloneOutcome`]; never prints, mutates the process CWD, or returns
/// `GitError::Exit`. A missing `refs/remotes/<origin>/<checkout_branch>` after the
/// fetch is reported as [`GitError::NotFound`] for the caller to map (the CLI
/// turns an explicit `--branch` miss into its own message).
pub fn clone(request: CloneRequest<'_>, services: CloneServices<'_>) -> Result<CloneOutcome> {
    let layout = RepositoryBootstrap::init(InitOptions {
        git_dir_override: request.git_dir_override.map(Path::to_path_buf),
        core_worktree: request.core_worktree.map(str::to_string),
        object_dir: None,
        worktree: request.destination.to_path_buf(),
        object_format: request.format,
        object_format_explicit: false,
        bare: false,
        initial_branch: CLONE_UNBORN_BRANCH.into(),
        template_dir: None,
        copy_template_config: false,
        separate_git_dir: None,
        shared_repository: None,
        ref_storage: request.options.ref_storage,
        ref_storage_explicit: request.options.ref_storage != RefStorageFormat::Files,
    })?;
    let git_dir = layout.git_dir;

    let config = (services.configure)(&git_dir)?;
    crate::protocol::check_transport_allowed(
        scheme_for_clone_source(request.source),
        Some(&config),
        None,
    )
    .map_err(crate::protocol::transport_policy_git_error)?;
    let fetch_source = match request.source {
        #[cfg(feature = "http")]
        CloneSource::Http(remote) => FetchSource::Http(remote.clone()),
        #[cfg(not(feature = "http"))]
        CloneSource::Http(_) => {
            return Err(GitError::Unsupported(
                "HTTP transport is not enabled in this build".into(),
            ));
        }
        CloneSource::Ssh(remote) => FetchSource::Ssh(remote.clone()),
        CloneSource::Git {
            remote,
            protocol_v2,
        } => FetchSource::Git {
            remote: remote.clone(),
            protocol_v2: *protocol_v2,
        },
        CloneSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => FetchSource::Local {
            git_dir: remote_git_dir.clone(),
            common_git_dir: remote_common_git_dir.clone(),
        },
    };
    let fetch_options = clone_fetch_options(CloneFetchOptions {
        depth: request.options.depth,
        deepen_since: request.options.deepen_since,
        deepen_not: request.options.deepen_not.clone(),
        filter: request.options.filter.clone(),
        record_promisor_refs: !request.options.checkout,
        reject_shallow: request.options.reject_shallow,
        ssh_options: request.options.ssh_options,
        upload_pack_command: request.options.upload_pack_command,
    });
    fetch(
        crate::fetch::FetchRequest {
            git_dir: &git_dir,
            format: request.format,
            config: &config,
            remote_name: request.options.origin,
            source: &fetch_source,
            refspecs: &[],
            options: &fetch_options,
        },
        crate::fetch::FetchServices {
            credentials: services.credentials,
            progress: services.progress,
            ref_hook: None,
        },
    )?;

    let store = FileRefStore::new(&git_dir, request.format);
    if let Some(detached) = &request.options.detached_head {
        write_clone_remote_head(&store, request.options)?;
        if request.options.checkout {
            sley_worktree::checkout_detached_filtered(
                request.destination,
                &git_dir,
                request.format,
                detached,
                request.options.committer.clone(),
                b"clone: checkout".to_vec(),
                &config,
            )?;
        } else {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".to_string(),
                expected: None,
                new: RefTarget::Direct(*detached),
                reflog: None,
            });
            tx.commit()?;
        }
        return Ok(CloneOutcome {
            git_dir,
            branch_oid: Some(*detached),
            empty: false,
        });
    }
    let remote_branch_ref = format!(
        "refs/remotes/{}/{}",
        request.options.origin, request.options.checkout_branch
    );
    let branch_oid = match store.read_ref(&remote_branch_ref)? {
        Some(RefTarget::Direct(oid)) => oid,
        Some(RefTarget::Symbolic(_)) => {
            return Err(GitError::Unsupported(
                "clone remote-tracking branch must be direct".into(),
            ));
        }
        None => {
            // The remote advertised no tip for the branch we are tracking. When
            // the caller did not request an explicit branch this is an
            // empty/unborn-repository clone: upstream `builtin/clone.c` warns,
            // skips the checkout, and leaves `HEAD` as an unborn symref pointing
            // at the remote's (or local default) branch — `update_head`'s
            // `unborn` arm. We mirror that by setting `HEAD` and returning a
            // marker for the CLI to print the warning. An explicit-branch miss
            // is still a hard error (the CLI maps it to git's "Remote branch …
            // not found" message).
            if request.options.branch_explicit {
                return Err(GitError::reference_not_found(format!(
                    "remote ref {remote_branch_ref}"
                )));
            }
            let unborn = format!("refs/heads/{}", request.options.checkout_branch);
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: "HEAD".to_string(),
                expected: None,
                new: RefTarget::Symbolic(unborn),
                reflog: None,
            });
            tx.commit()?;
            // Install branch upstream config for the unborn branch, matching
            // git's `install_branch_config` in the unborn path.
            (services.configure_branch)(&git_dir, request.options.checkout_branch)?;
            return Ok(CloneOutcome {
                git_dir,
                branch_oid: None,
                empty: true,
            });
        }
    };
    store.create_branch(
        request.options.checkout_branch,
        branch_oid.clone(),
        request.options.committer.clone(),
        format!(
            "branch: Created from {}/{}",
            request.options.origin, request.options.checkout_branch
        )
        .into_bytes(),
    )?;
    // The branch upstream config is written here and the resulting config is used
    // for the checkout below, matching the CLI's previous order: configure the
    // branch, point the remote `HEAD`, then read the (now final) config for the
    // smudge-side checkout filters. Pointing `HEAD` only updates refs, so it does
    // not change the config `configure_branch` returns.
    let checkout_config = (services.configure_branch)(&git_dir, request.options.checkout_branch)?;
    if request.options.checkout {
        fetch_partial_clone_checkout_blobs(&request, &git_dir, branch_oid, services.credentials)?;
    } else {
        let mut tx = store.transaction();
        tx.update(RefUpdate {
            name: "HEAD".to_string(),
            expected: None,
            new: RefTarget::Symbolic(format!("refs/heads/{}", request.options.checkout_branch)),
            reflog: None,
        });
        tx.commit()?;
    }
    write_clone_remote_head(&store, request.options)?;

    if request.options.checkout {
        sley_worktree::checkout_branch_filtered(
            request.destination,
            &git_dir,
            request.format,
            request.options.checkout_branch,
            request.options.committer.clone(),
            &checkout_config,
        )?;
    }

    Ok(CloneOutcome {
        git_dir,
        branch_oid: Some(branch_oid),
        empty: false,
    })
}

fn write_clone_remote_head(store: &FileRefStore, options: &CloneOptions<'_>) -> Result<()> {
    if options.remote_head_branch.is_empty()
        || (options.single_branch && options.checkout_branch != options.remote_head_branch)
    {
        return Ok(());
    }
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: format!("refs/remotes/{}/HEAD", options.origin),
        expected: None,
        new: RefTarget::Symbolic(format!(
            "refs/remotes/{}/{}",
            options.origin, options.remote_head_branch
        )),
        reflog: None,
    });
    tx.commit()
}

fn scheme_for_clone_source(source: &CloneSource) -> &'static str {
    match source {
        CloneSource::Http(remote) => crate::protocol::transport_scheme_for_remote(remote),
        CloneSource::Ssh(remote) => crate::protocol::transport_scheme_for_remote(remote),
        CloneSource::Git { remote, .. } => crate::protocol::transport_scheme_for_remote(remote),
        CloneSource::Local { .. } => "file",
    }
}

/// Materialize the blobs needed to check out `commit_oid` for a partial clone
/// that filtered them out of the initial fetch.
///
/// A `--filter=blob:limit=…`/`blob:none` clone omits blobs from the initial pack,
/// but the working-tree checkout still needs the blobs reachable from the checked
/// out commit's tree. Upstream lazily fetches those blobs from the promisor
/// remote during checkout; sley fetches them up front here (only the exact
/// checkout blobs, so unreachable filtered blobs stay absent). The trees are
/// already present locally (a blob filter keeps trees), so the wanted blob ids
/// are computed by walking the local copy of the commit's tree.
fn fetch_partial_clone_checkout_blobs(
    request: &CloneRequest<'_>,
    git_dir: &Path,
    commit_oid: ObjectId,
    credentials: &mut dyn CredentialProvider,
) -> Result<()> {
    if request.options.filter.is_none() {
        return Ok(());
    }
    match request.source {
        CloneSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => {
            let local_db = FileObjectDatabase::from_git_dir(git_dir, request.format);
            let remote_db = FileObjectDatabase::from_git_dir(remote_common_git_dir, request.format);
            let mut wants = Vec::new();
            let mut seen = std::collections::HashSet::new();
            collect_checkout_materialization_wants(
                &remote_db,
                &local_db,
                request.format,
                commit_oid,
                &mut seen,
                &mut wants,
            )?;
            crate::local::install_fetch_pack_via_local_upload_pack(
                git_dir,
                remote_git_dir,
                request.format,
                wants,
                None,
                true,
                false,
                None,
                None,
                false,
                None,
            )?;
            Ok(())
        }
        #[cfg(feature = "http")]
        CloneSource::Http(remote) => fetch_http_partial_clone_checkout_blobs(
            request,
            git_dir,
            commit_oid,
            remote,
            credentials,
        ),
        // SSH/git:// partial clones are gated out by the CLI (the in-process
        // local server is the only transport that advertises object filtering
        // aside from HTTP), so there is nothing to materialize here.
        _ => Ok(()),
    }
}

/// HTTP arm of [`fetch_partial_clone_checkout_blobs`]: fetch the checkout blobs
/// from the smart-HTTP promisor remote as a second, targeted fetch (requesting
/// the exact blob ids via `want <oid>`, which the server permits under
/// `uploadpack.allowanysha1inwant`). Installed as a `.promisor` pack.
#[cfg(feature = "http")]
fn fetch_http_partial_clone_checkout_blobs(
    request: &CloneRequest<'_>,
    git_dir: &Path,
    commit_oid: ObjectId,
    remote: &RemoteUrl,
    credentials: &mut dyn CredentialProvider,
) -> Result<()> {
    let local_db = FileObjectDatabase::from_git_dir(git_dir, request.format);
    let mut wants = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // The commit and its trees are present locally (only blobs were filtered),
    // so the local db serves as both the tree source and the presence check.
    collect_checkout_materialization_wants(
        &local_db,
        &local_db,
        request.format,
        commit_oid,
        &mut seen,
        &mut wants,
    )?;
    if wants.is_empty() {
        return Ok(());
    }
    let http_batch = crate::http::HttpOperationBatch::new();
    let client = http_batch.client();
    let discovered = crate::http::http_service_advertisements(
        client,
        remote,
        request.format,
        sley_protocol::GitService::UploadPack,
        credentials,
        None,
    )?;
    let git_protocol = crate::http::http_git_protocol_header_value(None)?;
    let pack_request = crate::http::HttpFetchPackRequest {
        client,
        git_dir,
        format: request.format,
        remote,
        wants,
        haves: None,
        shallow: Vec::new(),
        deepen: None,
        promisor: true,
        max_input_size: None,
        filter: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        deepen_relative: false,
        git_protocol: git_protocol.as_deref(),
        // The client already has the checkout commit and its trees; advertising
        // them as haves would make the server omit the (filtered-out) blobs we
        // are explicitly requesting, so suppress haves for this top-up fetch.
        omit_haves: true,
    };
    if let Some(handshake) = discovered.handshake.as_ref() {
        crate::http::install_fetch_pack_via_http_protocol_v2_fetch(
            pack_request,
            handshake,
            credentials,
        )?;
    } else {
        crate::http::install_fetch_pack_via_http_upload_pack(pack_request, credentials)?;
    }
    Ok(())
}

fn collect_checkout_materialization_wants(
    remote_db: &FileObjectDatabase,
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: ObjectId,
    seen: &mut std::collections::HashSet<ObjectId>,
    wants: &mut Vec<ObjectId>,
) -> Result<()> {
    let commit_object = remote_db.read_object(&commit_oid)?;
    if commit_object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            commit_object.object_type.as_str()
        )));
    }
    let commit = Commit::parse_ref(format, &commit_object.body)?;
    collect_tree_materialization_wants(remote_db, local_db, format, commit.tree, seen, wants)
}

fn collect_tree_materialization_wants(
    remote_db: &FileObjectDatabase,
    local_db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: ObjectId,
    seen: &mut std::collections::HashSet<ObjectId>,
    wants: &mut Vec<ObjectId>,
) -> Result<()> {
    if !seen.insert(tree_oid) {
        return Ok(());
    }
    if !local_db.contains(&tree_oid)? {
        wants.push(tree_oid);
    }
    let tree_object = remote_db.read_object(&tree_oid)?;
    if tree_object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            tree_object.object_type.as_str()
        )));
    }
    for entry in Tree::parse(format, &tree_object.body)?.entries {
        if entry.is_tree() {
            collect_tree_materialization_wants(
                remote_db, local_db, format, entry.oid, seen, wants,
            )?;
        } else if !entry.is_gitlink() && seen.insert(entry.oid) && !local_db.contains(&entry.oid)? {
            wants.push(entry.oid);
        }
    }
    Ok(())
}

/// The fixed [`FetchOptions`] a clone fetch uses: quiet, auto-follow tags, write
/// `FETCH_HEAD`, the requested shallow `depth`, and otherwise neutral (no prune, no
/// `--tags`, not a dry run, not appending). Mirrors the options the CLI's clone
/// paths passed.
struct CloneFetchOptions<'a> {
    depth: Option<u32>,
    deepen_since: Option<i64>,
    deepen_not: Vec<String>,
    filter: Option<sley_odb::PackObjectFilter>,
    record_promisor_refs: bool,
    reject_shallow: bool,
    ssh_options: Option<crate::ssh::SshTransportOptions>,
    upload_pack_command: Option<&'a str>,
}

fn clone_fetch_options(options: CloneFetchOptions<'_>) -> FetchOptions {
    let CloneFetchOptions {
        depth,
        deepen_since,
        deepen_not,
        filter,
        record_promisor_refs,
        reject_shallow,
        ssh_options,
        upload_pack_command,
    } = options;
    FetchOptions {
        quiet: true,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        prune_tags: false,
        dry_run: false,
        force: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        prune_tags_option_explicit: false,
        refmap: None,
        depth,
        merge_srcs: Vec::new(),
        filter,
        refetch: false,
        cloning: true,
        record_promisor_refs,
        update_shallow: false,
        reject_shallow,
        deepen_relative: false,
        update_head_ok: false,
        deepen_since,
        deepen_not,
        ssh_options,
        upload_pack_command: upload_pack_command.map(str::to_string),
        atomic: false,
        negotiation_restrict: None,
        negotiation_include: None,
    }
}
