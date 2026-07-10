//! Per-invocation CLI context (replaces process-global `GLOBAL_*` statics).
//!
//! Built once in [`crate::run`] after global option parsing; command code reads
//! overrides through [`cli_session`] and the `global_*` accessors in `lib.rs`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// When set, `global_git_dir` / `explicit_git_dir` return `None` (local
    /// transport subprocess isolation).
    pub local_repo_env_hidden: bool,
}

static CLI_SESSION: Mutex<Option<CliSession>> = Mutex::new(None);

pub(crate) fn install_cli_session(session: CliSession) {
    if let Ok(mut slot) = CLI_SESSION.lock() {
        *slot = Some(session);
    }
}

pub(crate) fn cli_session() -> Option<CliSession> {
    CLI_SESSION.lock().ok()?.clone()
}

pub(crate) fn with_local_repo_env_hidden_session<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let previous = CLI_SESSION.lock().ok().and_then(|mut slot| {
        slot.as_mut().map(|session| {
            let previous = session.local_repo_env_hidden;
            session.local_repo_env_hidden = true;
            previous
        })
    });
    let result = f();
    if let Some(previous) = previous
        && let Ok(mut slot) = CLI_SESSION.lock()
        && let Some(session) = slot.as_mut()
    {
        session.local_repo_env_hidden = previous;
    }
    result
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
        git_dir.clone(),
        work_tree.clone(),
        attr_source.clone(),
        bare,
        lazy_fetch,
        pathspec_flags,
    );
    let Ok(mut slot) = CLI_SESSION.lock() else {
        return;
    };
    let Some(installed) = slot.as_mut() else {
        return;
    };
    apply_global_overrides(
        installed,
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

    pub(crate) fn pathspec_flags(&self) -> PathspecFlags {
        self.env.pathspec_flags
    }

    /// Refresh the effective cwd after alias-expanded `-C` options are parsed.
    pub(crate) fn refresh_cwd(&mut self) {
        let Ok(cwd) = env::current_dir() else {
            return;
        };
        self.cwd = cwd.clone();
        if let Ok(mut slot) = CLI_SESSION.lock()
            && let Some(installed) = slot.as_mut()
        {
            installed.cwd = cwd;
        }
    }

    /// Open a facade repository using the CLI's resolved git-directory rules.
    ///
    /// Discovery (including `--git-dir`, `GIT_DIR`, gitfiles, bare mode, and
    /// ownership checks) remains a CLI concern. Once resolved, the engine
    /// facade owns the repository-scoped object database and other handles.
    pub(crate) fn open_repository(&self) -> Result<Repository> {
        let git_dir = self.git_dir()?;
        let config = crate::read_repo_config(&git_dir)?;
        let use_replace_refs = config
            .get_bool("core", None, "useReplaceRefs")
            .unwrap_or(true);
        Repository::open_with(
            git_dir,
            OpenOptions::new()
                .exact_path(true)
                .replace_objects(self.replace_objects())
                .effective_use_replace_refs(use_replace_refs),
        )
    }

    /// Resolved git directory for this session's cwd.
    pub(crate) fn git_dir(&self) -> Result<PathBuf> {
        let cwd = self.cwd.clone();
        if !self.local_repo_env_hidden()
            && let Some(git_dir) = self
                .git_dir_override()
                .or_else(|| env::var_os("GIT_DIR").map(PathBuf::from))
        {
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
        if !self.local_repo_env_hidden() && self.bare() {
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

/// Resolve the git directory for the active session's cwd.
pub(crate) fn cli_git_dir() -> Result<PathBuf> {
    let cwd = cli_session()
        .map(|session| session.cwd)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    cli_git_dir_from(cwd)
}

/// Resolve the git directory from `start`, honouring session overrides.
pub(crate) fn cli_git_dir_from(start: impl AsRef<Path>) -> Result<PathBuf> {
    discovery::resolve_git_dir(start)
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
}
