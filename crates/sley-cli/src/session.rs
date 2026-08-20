//! Per-invocation CLI context (replaces process-global `GLOBAL_*` statics).
//!
//! Built once in [`crate::run`] after global option parsing and passed through
//! dispatch explicitly.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use sley::{GitConfig, GitError, ObjectFormat, OpenOptions, Repository, Result};

use crate::PathspecFlags;
use crate::discovery;

/// Flags and env overrides applied before command dispatch (`--git-dir`,
/// `--work-tree`, `--bare`, pathspec magic, etc.).
#[derive(Debug, Clone)]
pub(crate) struct CliEnv {
    pub git_dir: Option<PathBuf>,
    pub work_tree: Option<PathBuf>,
    pub attr_source: Option<String>,
    pub bare: bool,
    pub replace_objects: bool,
    pub lazy_fetch: bool,
    pub pathspec_flags: PathspecFlags,
}

/// One CLI invocation's repository and environment state.
#[derive(Debug, Clone)]
pub(crate) struct CliSession {
    pub cwd: PathBuf,
    pub env: CliEnv,
    /// When set, repository environment overrides are hidden from a child
    /// invocation (local transport subprocess isolation).
    pub local_repo_env_hidden: bool,
    repository_snapshot: OnceLock<Result<Arc<InvocationRepositorySnapshot>>>,
}

/// Repository state resolved once for one immutable invocation context.
#[derive(Debug, Clone)]
pub(crate) struct InvocationRepositorySnapshot {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    pub linked_worktree: bool,
    pub branch: Option<String>,
    pub config: GitConfig,
    pub setup_config: GitConfig,
    pub format: ObjectFormat,
}

/// Apply alias-expanded leading global options onto the active session.
pub(crate) fn merge_global_overrides(
    session: &mut CliSession,
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    attr_source: Option<String>,
    bare: bool,
    lazy_fetch: bool,
    pathspec_flags: PathspecFlags,
) {
    apply_global_overrides(
        session,
        git_dir,
        work_tree,
        attr_source,
        bare,
        lazy_fetch,
        pathspec_flags,
    );
}

fn apply_global_overrides(
    session: &mut CliSession,
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    attr_source: Option<String>,
    bare: bool,
    lazy_fetch: bool,
    pathspec_flags: PathspecFlags,
) {
    let mut repository_inputs_changed = false;
    if git_dir.is_some() {
        session.env.git_dir = git_dir;
        repository_inputs_changed = true;
    }
    if work_tree.is_some() {
        session.env.work_tree = work_tree;
        repository_inputs_changed = true;
    }
    if attr_source.is_some() {
        session.env.attr_source = attr_source;
    }
    if bare {
        session.env.bare = true;
        repository_inputs_changed = true;
    }
    if !lazy_fetch {
        session.env.lazy_fetch = false;
    }
    if pathspec_flags != PathspecFlags::default() {
        session.env.pathspec_flags = pathspec_flags;
    }
    if repository_inputs_changed {
        session.invalidate_repository_snapshot();
    }
}

impl CliSession {
    pub(crate) fn from_parsed_globals(
        cwd: PathBuf,
        git_dir: Option<PathBuf>,
        work_tree: Option<PathBuf>,
        attr_source: Option<String>,
        bare: bool,
        replace_objects: bool,
        lazy_fetch: bool,
        pathspec_flags: PathspecFlags,
    ) -> Self {
        Self {
            cwd,
            env: CliEnv {
                git_dir,
                work_tree,
                attr_source,
                bare,
                replace_objects,
                lazy_fetch,
                pathspec_flags,
            },
            local_repo_env_hidden: false,
            repository_snapshot: OnceLock::new(),
        }
    }

    /// Resolve and cache the repository/config bootstrap for this invocation.
    pub(crate) fn repository_snapshot(&self) -> Result<Arc<InvocationRepositorySnapshot>> {
        match self.repository_snapshot.get_or_init(|| {
            let git_dir = self.git_dir()?;
            if self.explicit_git_dir().is_some() && !discovery::is_git_dir_candidate(&git_dir) {
                return Err(GitError::repository_not_found(format!(
                    "not a git repository: {}",
                    git_dir.display()
                )));
            }
            let common = crate::repo_paths::common_git_dir_snapshot_with_env(
                &git_dir,
                !self.local_repo_env_hidden(),
            )?;
            let common_dir = common.path;
            let linked_worktree = common.linked_worktree;
            let branch = crate::commands::remote::repo_current_branch_name(&git_dir);
            let (config, setup_config, format) =
                crate::commands::remote::read_effective_repo_snapshot_resolved(
                    &git_dir,
                    &common_dir,
                    branch.clone(),
                    self.cwd(),
                )?;
            Ok(Arc::new(InvocationRepositorySnapshot {
                git_dir,
                common_dir,
                linked_worktree,
                branch,
                config,
                setup_config,
                format,
            }))
        }) {
            Ok(snapshot) => {
                sley::plumbing::sley_core::activate_precompose_unicode(snapshot.config.get_bool(
                    "core",
                    None,
                    "precomposeunicode",
                ));
                Ok(Arc::clone(snapshot))
            }
            Err(err) => {
                sley::plumbing::sley_core::activate_precompose_unicode(None);
                Err(err.clone())
            }
        }
    }

    fn invalidate_repository_snapshot(&mut self) {
        let _ = self.repository_snapshot.take();
    }

    pub(crate) fn git_dir_override(&self) -> Option<PathBuf> {
        if self.local_repo_env_hidden {
            return None;
        }
        self.env.git_dir.clone()
    }

    pub(crate) fn work_tree_override(&self) -> Option<PathBuf> {
        if self.local_repo_env_hidden {
            return None;
        }
        self.env.work_tree.clone()
    }

    /// Invocation-effective `GIT_DIR`, including the parsed CLI override.
    pub(crate) fn explicit_git_dir(&self) -> Option<PathBuf> {
        self.git_dir_override().or_else(|| {
            if self.local_repo_env_hidden {
                None
            } else {
                env::var_os("GIT_DIR").map(PathBuf::from)
            }
        })
    }

    /// Invocation-effective `GIT_WORK_TREE`, including the parsed CLI override.
    pub(crate) fn explicit_work_tree(&self) -> Option<PathBuf> {
        self.work_tree_override().or_else(|| {
            if self.local_repo_env_hidden {
                None
            } else {
                env::var_os("GIT_WORK_TREE").map(PathBuf::from)
            }
        })
    }

    /// Whether this invocation explicitly requested a bare repository.
    pub(crate) fn explicit_bare(&self) -> bool {
        !self.local_repo_env_hidden && self.bare()
    }

    pub(crate) fn attr_source(&self) -> Option<String> {
        self.env.attr_source.clone()
    }

    pub(crate) fn bare(&self) -> bool {
        self.env.bare
    }

    pub(crate) fn replace_objects(&self) -> bool {
        self.env.replace_objects
    }

    pub(crate) fn lazy_fetch(&self) -> bool {
        self.env.lazy_fetch
    }

    pub(crate) fn local_repo_env_hidden(&self) -> bool {
        self.local_repo_env_hidden
    }

    /// Derive a child invocation which must not inherit the parent repository.
    ///
    /// Local transport and submodule-clone paths use this instead of mutating
    /// the process-global compatibility session around an in-process command.
    pub(crate) fn local_repo_env_hidden_child(&self) -> Self {
        let mut child = self.clone();
        child.local_repo_env_hidden = true;
        child.invalidate_repository_snapshot();
        child
    }

    /// Create an isolated child invocation rooted at `cwd` with no parent
    /// repository overrides. Used by recursive engine callbacks which have an
    /// explicit worktree path but no need to inherit the caller's repository.
    pub(crate) fn isolated_child(cwd: PathBuf) -> Self {
        let mut child = Self::from_parsed_globals(
            cwd,
            None,
            None,
            None,
            false,
            true,
            true,
            PathspecFlags::default(),
        );
        child.local_repo_env_hidden = true;
        child
    }

    /// Build an invocation context pinned to an already-resolved repository.
    /// Used by nested in-process engine calls that must not rediscover cwd.
    pub(crate) fn for_repository_paths(cwd: PathBuf, git_dir: PathBuf) -> Self {
        Self::from_parsed_globals(
            cwd,
            Some(git_dir),
            None,
            None,
            false,
            true,
            true,
            PathspecFlags::default(),
        )
    }

    pub(crate) fn pathspec_flags(&self) -> PathspecFlags {
        self.env.pathspec_flags
    }

    /// Refresh the effective cwd after alias-expanded `-C` options are parsed.
    pub(crate) fn refresh_cwd(&mut self) {
        self.invalidate_repository_snapshot();
        let Ok(cwd) = env::current_dir() else {
            return;
        };
        self.cwd = cwd;
    }

    /// Open a facade repository using the CLI's resolved git-directory rules.
    ///
    /// Discovery (including `--git-dir`, `GIT_DIR`, gitfiles, bare mode, and
    /// ownership checks) remains a CLI concern. Once resolved, the engine
    /// facade owns the repository-scoped object database and other handles.
    pub(crate) fn open_repository(&self) -> Result<Repository> {
        let git_dir = self.git_dir()?;
        let work_tree = self.effective_worktree_for_git_dir(&git_dir)?;
        let config = crate::read_repo_config(&git_dir)?;
        let use_replace_refs = config
            .get_bool("core", None, "useReplaceRefs")
            .unwrap_or(true);
        let repository = Repository::open_with(
            git_dir,
            OpenOptions::new()
                .exact_path(true)
                .replace_objects(self.replace_objects())
                .effective_use_replace_refs(use_replace_refs),
        )?;
        Ok(match work_tree {
            Some(work_tree) => repository.with_work_tree(work_tree),
            None => repository,
        })
    }

    pub(crate) fn common_git_dir(&self, git_dir: &Path) -> Result<PathBuf> {
        crate::repo_paths::common_git_dir_for_git_dir_with_env(
            git_dir,
            !self.local_repo_env_hidden(),
        )
    }

    /// Resolve the worktree for `git_dir` using this invocation's location policy.
    pub(crate) fn worktree_root_for_git_dir(&self, git_dir: &Path) -> Result<PathBuf> {
        self.effective_worktree_for_git_dir(git_dir)?
            .ok_or_else(|| {
                GitError::Unsupported("update-index currently requires a non-bare worktree".into())
            })
    }

    /// Resolve the invocation's effective worktree without requiring one.
    /// Read-only commands use this to distinguish a bare repository from a
    /// bare repository paired with an explicit `--work-tree`.
    pub(crate) fn optional_worktree_for_git_dir(&self, git_dir: &Path) -> Result<Option<PathBuf>> {
        self.effective_worktree_for_git_dir(git_dir)
    }

    /// Resolve the invocation worktree from already-loaded physical and
    /// effective config snapshots without re-opening repository config.
    pub(crate) fn optional_worktree_from_config(
        &self,
        git_dir: &Path,
        setup_config: &GitConfig,
        effective_config: &GitConfig,
        linked_worktree: bool,
        policy: crate::repository::WorktreePolicy,
    ) -> Result<Option<PathBuf>> {
        if let Some(work_tree) = self.explicit_work_tree() {
            let work_tree =
                discovery::resolve_cli_path(&self.cwd, work_tree.to_string_lossy().as_ref());
            return fs::canonicalize(work_tree)
                .map(Some)
                .map_err(|err| GitError::Io(err.to_string()));
        }

        match policy {
            crate::repository::WorktreePolicy::Command => self
                .optional_command_worktree_from_config(
                    git_dir,
                    setup_config,
                    effective_config,
                    linked_worktree,
                ),
            crate::repository::WorktreePolicy::HashAttributes => self
                .optional_hash_attribute_root_from_config(
                    git_dir,
                    setup_config,
                    effective_config,
                    linked_worktree,
                ),
        }
    }

    fn optional_command_worktree_from_config(
        &self,
        git_dir: &Path,
        setup_config: &GitConfig,
        effective_config: &GitConfig,
        linked_worktree: bool,
    ) -> Result<Option<PathBuf>> {
        if self.explicit_git_dir().is_some() {
            if effective_config.get_bool("core", None, "bare") == Some(true) && !linked_worktree {
                return Ok(None);
            }
            if let Some(worktree) = effective_config.get("core", None, "worktree") {
                return canonicalize_configured_worktree(git_dir, worktree).map(Some);
            }
            if implicit_worktree_disabled() {
                return Ok(None);
            }
            return canonicalize_cwd(&self.cwd).map(Some);
        }
        if self.explicit_bare() {
            return Ok(None);
        }
        if linked_worktree && let Some(worktree) = linked_worktree_root(git_dir)? {
            return Ok(Some(worktree));
        }
        if setup_config.get_bool("core", None, "bare") == Some(true) {
            return Ok(None);
        }
        if let Some(worktree) = setup_config.get("core", None, "worktree") {
            return canonicalize_configured_worktree(git_dir, worktree).map(Some);
        }
        if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
            return git_dir
                .parent()
                .map(Path::to_path_buf)
                .map(Some)
                .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()));
        }
        Ok(crate::setup::setup_git_directory(self).and_then(|setup| setup.worktree))
    }

    fn optional_hash_attribute_root_from_config(
        &self,
        git_dir: &Path,
        setup_config: &GitConfig,
        effective_config: &GitConfig,
        linked_worktree: bool,
    ) -> Result<Option<PathBuf>> {
        // A linked worktree's backlink is intrinsic layout. It wins even when
        // common/effective core.bare is true or implicit worktrees are disabled.
        if linked_worktree && let Some(worktree) = linked_worktree_root(git_dir)? {
            return Ok(Some(worktree));
        }

        let physical_bare = setup_config.get_bool("core", None, "bare") == Some(true);
        if !physical_bare {
            if let Some(worktree) = setup_config.get("core", None, "worktree") {
                return canonicalize_configured_worktree(git_dir, worktree).map(Some);
            }
            if git_dir.file_name().and_then(|name| name.to_str()) == Some(".git") {
                return git_dir
                    .parent()
                    .map(Path::to_path_buf)
                    .map(Some)
                    .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()));
            }

            // Preserve the non-standard gitfile fallback for a physically
            // non-bare admin directory without a normal backlink.
            return Ok(crate::setup::setup_git_directory(self).and_then(|setup| setup.worktree));
        }

        if effective_config.get_bool("core", None, "bare") != Some(false) {
            return Ok(None);
        }
        // For hash-object attributes Git uses cwd here, ignoring config
        // core.worktree and GIT_IMPLICIT_WORK_TREE. An explicit work-tree was
        // already handled by the caller above.
        canonicalize_cwd(&self.cwd).map(Some)
    }

    /// Resolve Git's effective worktree policy without requiring one to exist.
    ///
    /// An explicit git directory changes the implicit worktree from the
    /// repository-intrinsic `.git` parent to the invocation cwd. Delegate that
    /// distinction (including `core.worktree`, `core.bare`, and
    /// `GIT_IMPLICIT_WORK_TREE`) to the shared setup engine.
    fn effective_worktree_for_git_dir(&self, git_dir: &Path) -> Result<Option<PathBuf>> {
        if let Some(work_tree) = self.explicit_work_tree() {
            let work_tree =
                discovery::resolve_cli_path(&self.cwd, work_tree.to_string_lossy().as_ref());
            return fs::canonicalize(work_tree)
                .map(Some)
                .map_err(|err| GitError::Io(err.to_string()));
        }
        if self.explicit_git_dir().is_some() {
            let setup = crate::setup::setup_git_directory(self).ok_or_else(|| {
                GitError::repository_not_found(format!(
                    "not a git repository: {}",
                    git_dir.display()
                ))
            })?;
            return Ok(setup.worktree);
        }
        if self.explicit_bare() {
            return Ok(None);
        }
        if let Some(root) = crate::sley_worktree::worktree_root_for_git_dir(git_dir)? {
            return Ok(Some(root));
        }
        // Intrinsic layout only recognizes a worktree when the admin dir is
        // named `.git` (or has a linked-worktree `gitdir`/`commondir` pair).
        // A gitfile that points at a differently-named directory (e.g. the
        // t2105 `gitdir: .real` layout) has a real worktree at the gitfile's
        // parent; fall back to CLI setup discovery so `add` / `commit` work.
        if let Some(setup) = crate::setup::setup_git_directory(self) {
            return Ok(setup.worktree);
        }
        Ok(None)
    }

    /// Resolved git directory for this session's cwd.
    pub(crate) fn git_dir(&self) -> Result<PathBuf> {
        let cwd = self.cwd.clone();
        if let Some(git_dir) = self.explicit_git_dir() {
            if git_dir.as_os_str().is_empty() {
                return Err(GitError::repository_not_found("not a git repository"));
            }
            let resolved = discovery::resolve_cli_path(&cwd, git_dir.to_string_lossy().as_ref());
            if resolved.is_file()
                && let Some(target) = discovery::read_gitdir_file(&resolved)?
                && discovery::is_git_dir_candidate(&target)
            {
                return fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()));
            }
            return Ok(resolved);
        }
        if self.explicit_bare() {
            if discovery::is_git_dir_candidate(&cwd) {
                return fs::canonicalize(cwd).map_err(|err| GitError::Io(err.to_string()));
            }
            return Err(GitError::repository_not_found("not a git repository"));
        }
        discovery::resolve_git_dir_walk_only(cwd)
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }
}

fn canonicalize_configured_worktree(git_dir: &Path, worktree: &str) -> Result<PathBuf> {
    let worktree = PathBuf::from(worktree);
    let worktree = if worktree.is_absolute() {
        worktree
    } else {
        git_dir.join(worktree)
    };
    fs::canonicalize(worktree).map_err(|err| GitError::Io(err.to_string()))
}

fn canonicalize_cwd(cwd: &Path) -> Result<PathBuf> {
    fs::canonicalize(cwd).map_err(|err| GitError::Io(err.to_string()))
}

fn implicit_worktree_disabled() -> bool {
    env::var_os("GIT_IMPLICIT_WORK_TREE").is_some_and(|value| {
        matches!(
            value.to_string_lossy().as_ref(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

fn linked_worktree_root(git_dir: &Path) -> Result<Option<PathBuf>> {
    let backlink = git_dir.join("gitdir");
    let Ok(value) = fs::read_to_string(&backlink) else {
        return Ok(None);
    };
    let path = PathBuf::from(value.trim());
    let gitfile = if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    };
    let Some(worktree) = gitfile.parent() else {
        return Ok(None);
    };
    fs::canonicalize(worktree)
        .map(Some)
        .map_err(|err| GitError::Io(err.to_string()))
}

/// Walk-up discovery without session overrides (local-path remotes).
pub(crate) fn cli_remote_git_dir_from(start: impl AsRef<Path>) -> Result<PathBuf> {
    discovery::resolve_git_dir_walk_only(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(cwd: PathBuf, git_dir: Option<PathBuf>) -> CliSession {
        CliSession::from_parsed_globals(
            cwd,
            git_dir,
            None,
            None,
            false,
            true,
            true,
            PathspecFlags::default(),
        )
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        env::temp_dir().join(format!("sley-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn local_session_receives_alias_expanded_overrides() {
        let mut session = session(env::temp_dir(), Some(PathBuf::from("one")));
        merge_global_overrides(
            &mut session,
            Some(PathBuf::from("two")),
            Some(PathBuf::from("worktree")),
            None,
            false,
            false,
            PathspecFlags {
                literal: false,
                glob: true,
                icase: false,
                literal_pathspecs: false,
            },
        );

        assert_eq!(session.git_dir_override(), Some(PathBuf::from("two")));
        assert_eq!(
            session.work_tree_override(),
            Some(PathBuf::from("worktree"))
        );
        assert!(!session.lazy_fetch());
        assert!(session.pathspec_flags().glob);
    }

    #[test]
    fn open_repository_uses_explicit_session_without_global_install() {
        let root = unique_temp_dir("explicit-session-open");
        let initialized = Repository::init(&root).expect("initialize repository");
        let session = session(root.clone(), Some(PathBuf::from(".git")));

        let opened = session.open_repository().expect("open repository");
        assert_eq!(
            fs::canonicalize(opened.git_dir()).expect("canonical opened git dir"),
            fs::canonicalize(initialized.git_dir()).expect("canonical initialized git dir")
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn open_repository_carries_explicit_git_dir_implicit_worktree() {
        let root = unique_temp_dir("explicit-session-worktree");
        let repository_root = root.join("repository");
        let invocation_cwd = root.join("input");
        fs::create_dir_all(&invocation_cwd).expect("create invocation cwd");
        let initialized = Repository::init(&repository_root).expect("initialize repository");
        let session = session(
            invocation_cwd.clone(),
            Some(initialized.git_dir().to_path_buf()),
        );

        let opened = session.open_repository().expect("open repository");
        assert_eq!(
            opened.workdir().expect("effective worktree"),
            fs::canonicalize(&invocation_cwd).expect("canonical invocation cwd")
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn repository_snapshot_is_reused_within_one_invocation() {
        let root = unique_temp_dir("session-snapshot-reuse");
        Repository::init(&root).expect("initialize repository");
        let session = session(root.clone(), None);

        let first = session.repository_snapshot().expect("first snapshot");
        let second = session.repository_snapshot().expect("cached snapshot");
        assert!(Arc::ptr_eq(&first, &second));

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn repository_snapshot_is_invalidated_by_repository_overrides_and_cwd_refresh() {
        let root = unique_temp_dir("session-snapshot-invalidation");
        let first_root = root.join("first");
        let second_root = root.join("second");
        Repository::init(&first_root).expect("initialize first repository");
        let second = Repository::init(&second_root).expect("initialize second repository");
        let mut session = session(first_root.clone(), None);

        let first = session.repository_snapshot().expect("first snapshot");
        merge_global_overrides(
            &mut session,
            Some(second.git_dir().to_path_buf()),
            None,
            None,
            false,
            true,
            PathspecFlags::default(),
        );
        let overridden = session.repository_snapshot().expect("overridden snapshot");
        assert!(!Arc::ptr_eq(&first, &overridden));
        assert_eq!(
            fs::canonicalize(&overridden.git_dir).expect("canonical overridden git dir"),
            fs::canonicalize(second.git_dir()).expect("canonical second git dir")
        );

        assert!(session.repository_snapshot.get().is_some());
        session.refresh_cwd();
        assert!(session.repository_snapshot.get().is_none());

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn repository_snapshot_caches_common_and_worktree_config_errors() {
        let root = unique_temp_dir("session-snapshot-config-errors");

        let common_root = root.join("common-malformed");
        let common_repo = Repository::init(&common_root).expect("initialize common fixture");
        let common_config = common_repo.git_dir().join("config");
        fs::write(&common_config, b"[core\n").expect("write malformed common config");
        let common_session = session(common_root, None);
        let first = common_session
            .repository_snapshot()
            .expect_err("malformed common config must fail");
        fs::write(&common_config, b"[core]\n\tbare = false\n").expect("repair common config");
        let cached = common_session
            .repository_snapshot()
            .expect_err("cached common config error must remain selected");
        assert_eq!(cached, first);

        let worktree_root = root.join("worktree-malformed");
        let worktree_repo = Repository::init(&worktree_root).expect("initialize worktree fixture");
        fs::write(
            worktree_repo.git_dir().join("config"),
            b"[core]\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n",
        )
        .expect("enable worktree config");
        let worktree_config = worktree_repo.git_dir().join("config.worktree");
        fs::write(&worktree_config, b"[core\n").expect("write malformed worktree config");
        let worktree_session = session(worktree_root, None);
        let first = worktree_session
            .repository_snapshot()
            .expect_err("malformed worktree config must fail");
        fs::write(&worktree_config, b"[core]\n\tbare = false\n").expect("repair worktree config");
        let cached = worktree_session
            .repository_snapshot()
            .expect_err("cached worktree config error must remain selected");
        assert_eq!(cached, first);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn cached_snapshot_selection_restores_precompose_policy() {
        let root = unique_temp_dir("session-snapshot-precompose-selection");
        let enabled_root = root.join("enabled");
        let disabled_root = root.join("disabled");
        let outside_root = root.join("outside");
        let enabled = Repository::init(&enabled_root).expect("initialize enabled fixture");
        Repository::init(&disabled_root).expect("initialize disabled fixture");
        fs::create_dir_all(&outside_root).expect("create outside fixture");
        fs::write(
            enabled.git_dir().join("config"),
            b"[core]\n\tbare = false\n\tprecomposeUnicode = true\n",
        )
        .expect("enable precompose");

        let parent = session(disabled_root, Some(enabled.git_dir().to_path_buf()));
        parent.repository_snapshot().expect("select enabled parent");
        assert!(sley::plumbing::sley_core::precompose_unicode_enabled());

        let child = parent.local_repo_env_hidden_child();
        child.repository_snapshot().expect("select disabled child");
        assert!(!sley::plumbing::sley_core::precompose_unicode_enabled());

        parent
            .repository_snapshot()
            .expect("reselect cached parent");
        assert!(sley::plumbing::sley_core::precompose_unicode_enabled());

        let outside = session(outside_root, None);
        outside
            .repository_snapshot()
            .expect_err("outside fixture has no repository");
        assert!(!sley::plumbing::sley_core::precompose_unicode_enabled());

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
