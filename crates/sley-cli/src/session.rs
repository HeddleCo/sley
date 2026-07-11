//! Per-invocation CLI context (replaces process-global `GLOBAL_*` statics).
//!
//! Built once in [`crate::run`] after global option parsing and passed through
//! dispatch explicitly.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sley::{GitConfig, GitError, OpenOptions, Repository, Result};

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
    if git_dir.is_some() {
        session.env.git_dir = git_dir;
    }
    if work_tree.is_some() {
        session.env.work_tree = work_tree;
    }
    if attr_source.is_some() {
        session.env.attr_source = attr_source;
    }
    if bare {
        session.env.bare = true;
    }
    if !lazy_fetch {
        session.env.lazy_fetch = false;
    }
    if pathspec_flags != PathspecFlags::default() {
        session.env.pathspec_flags = pathspec_flags;
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
        }
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

/// Walk-up discovery without session overrides (local-path remotes).
pub(crate) fn cli_remote_git_dir_from(start: impl AsRef<Path>) -> Result<PathBuf> {
    discovery::resolve_git_dir_walk_only(start)
}

/// Convenience for commands that already have a loaded config snapshot.
impl CliSession {
    pub(crate) fn with_config<'a>(&'a self, config: &'a GitConfig) -> CliSessionView<'a> {
        CliSessionView {
            session: self,
            config,
        }
    }
}

pub(crate) struct CliSessionView<'a> {
    pub session: &'a CliSession,
    pub config: &'a GitConfig,
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
}
