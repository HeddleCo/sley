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
//! SSH clone still lives in the CLI; only HTTP and local move here.

use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_formats::RepositoryLayout;
use sley_refs::{FileRefStore, RefTarget, RefUpdate};
use sley_transport::RemoteUrl;

use crate::fetch::{fetch, FetchOptions, FetchSource};
use crate::{CredentialProvider, ProgressSink};

/// The unborn placeholder branch the destination is initialized on, replaced by
/// the real checked-out branch; mirrors the CLI's previous clone init.
const CLONE_UNBORN_BRANCH: &str = "__git_rs_clone_unborn__";

/// How [`clone`] reaches the remote it is cloning from.
///
/// The caller resolves the remote (URL rewriting, repository discovery — all
/// process-state dependent) and hands `clone` a concrete transport.
pub enum CloneSource {
    /// A smart-HTTP(S) remote at the given already-resolved URL.
    Http(RemoteUrl),
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
    /// writing `$GIT_DIR/shallow`. `None` is a full clone. Honored only for the
    /// HTTP (and SSH) transports; a depth on a local clone is ignored upstream of
    /// `clone` by the caller (the in-process server has no shallow support).
    pub depth: Option<u32>,
    /// The committer identity for the branch-creation and checkout reflog entries.
    pub committer: Vec<u8>,
}

/// The structured result of a [`clone`].
#[derive(Debug, Clone)]
pub struct CloneOutcome {
    /// The destination repository's `$GIT_DIR` (the `.git` directory created by
    /// the init step). The caller uses it for its post-checkout steps.
    pub git_dir: PathBuf,
    /// The object id the local branch was created at (the fetched remote tip).
    pub branch_oid: ObjectId,
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
#[allow(clippy::too_many_arguments)]
pub fn clone(
    destination: &Path,
    format: ObjectFormat,
    source: &CloneSource,
    options: &CloneOptions,
    configure: &mut dyn FnMut(&Path) -> Result<GitConfig>,
    configure_branch: &mut dyn FnMut(&Path, &str) -> Result<GitConfig>,
    credentials: &mut dyn CredentialProvider,
    progress: &mut dyn ProgressSink,
) -> Result<CloneOutcome> {
    let layout = RepositoryLayout::init_at_with_initial_branch(
        destination,
        format,
        false,
        CLONE_UNBORN_BRANCH,
    )?;
    let git_dir = layout.git_dir;

    let config = configure(&git_dir)?;
    let fetch_source = match source {
        CloneSource::Http(remote) => FetchSource::Http(remote.clone()),
        CloneSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        } => FetchSource::Local {
            git_dir: remote_git_dir.clone(),
            common_git_dir: remote_common_git_dir.clone(),
        },
    };
    fetch(
        &git_dir,
        format,
        &config,
        options.origin,
        &fetch_source,
        &[],
        &clone_fetch_options(options.depth),
        credentials,
        progress,
    )?;

    let store = FileRefStore::new(&git_dir, format);
    let remote_branch_ref = format!(
        "refs/remotes/{}/{}",
        options.origin, options.checkout_branch
    );
    let branch_oid = match store.read_ref(&remote_branch_ref)? {
        Some(RefTarget::Direct(oid)) => oid,
        Some(RefTarget::Symbolic(_)) => {
            return Err(GitError::Unsupported(
                "clone remote-tracking branch must be direct".into(),
            ));
        }
        None => {
            return Err(GitError::NotFound(format!(
                "remote ref {remote_branch_ref}"
            )));
        }
    };
    store.create_branch(
        options.checkout_branch,
        branch_oid.clone(),
        options.committer.clone(),
        format!(
            "branch: Created from {}/{}",
            options.origin, options.checkout_branch
        )
        .into_bytes(),
    )?;
    // The branch upstream config is written here and the resulting config is used
    // for the checkout below, matching the CLI's previous order: configure the
    // branch, point the remote `HEAD`, then read the (now final) config for the
    // smudge-side checkout filters. Pointing `HEAD` only updates refs, so it does
    // not change the config `configure_branch` returns.
    let checkout_config = configure_branch(&git_dir, options.checkout_branch)?;
    if !options.single_branch || options.checkout_branch == options.remote_head_branch {
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
        tx.commit()?;
    }

    sley_worktree::checkout_branch_filtered(
        destination,
        &git_dir,
        format,
        options.checkout_branch,
        options.committer.clone(),
        &checkout_config,
    )?;

    Ok(CloneOutcome {
        git_dir,
        branch_oid,
    })
}

/// The fixed [`FetchOptions`] a clone fetch uses: quiet, auto-follow tags, write
/// `FETCH_HEAD`, the requested shallow `depth`, and otherwise neutral (no prune, no
/// `--tags`, not a dry run, not appending). Mirrors the options the CLI's clone
/// paths passed.
fn clone_fetch_options(depth: Option<u32>) -> FetchOptions {
    FetchOptions {
        quiet: true,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        depth,
    }
}
