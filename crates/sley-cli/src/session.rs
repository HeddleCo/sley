//! Per-invocation CLI context (replaces process-global `GLOBAL_*` statics).
//!
//! Built once in [`crate::run`] after global option parsing; command code reads
//! overrides through [`cli_session`] and the `global_*` accessors in `lib.rs`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sley_config::GitConfig;
use sley_core::Result;

use crate::repository::RepositoryContext;
use crate::{PathspecFlags, discover_git_dir};

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
            let prev = session.local_repo_env_hidden;
            session.local_repo_env_hidden = true;
            prev
        })
    });
    let result = f();
    if let Some(prev) = previous
        && let Ok(mut slot) = CLI_SESSION.lock()
        && let Some(session) = slot.as_mut()
    {
        session.local_repo_env_hidden = prev;
    }
    result
}

/// Apply alias-expanded leading global options onto the active session.
pub(crate) fn merge_global_overrides(
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    attr_source: Option<String>,
    bare: bool,
    lazy_fetch: bool,
    pathspec_flags: PathspecFlags,
) {
    let Ok(mut slot) = CLI_SESSION.lock() else {
        return;
    };
    let Some(session) = slot.as_mut() else {
        return;
    };
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

    /// Open the repository for the current cwd, honouring CLI/env overrides.
    pub(crate) fn open_repo(&self) -> Result<RepositoryContext> {
        RepositoryContext::discover(&self.cwd)
    }

    /// Best-effort git dir for the current cwd (optional commands).
    pub(crate) fn discover_git_dir(&self) -> Result<PathBuf> {
        discover_git_dir(&self.cwd)
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }
}

/// Convenience for commands that already have a loaded config snapshot.
impl CliSession {
    pub(crate) fn with_config<'a>(&'a self, config: &'a GitConfig) -> CliSessionView<'a> {
        CliSessionView { session: self, config }
    }
}

pub(crate) struct CliSessionView<'a> {
    pub session: &'a CliSession,
    pub config: &'a GitConfig,
}