//! Shared repository handles for command implementations.
//!
//! Commands should be able to say "open the repository" once and then reuse the
//! resulting object database, refs, config, and format. This keeps Git discovery
//! behavior in one place while preserving command-specific parsing and errors.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sley_config::GitConfig;
use sley_core::{ObjectFormat, Result};
use sley_odb::FileObjectDatabase;
use sley_refs::FileRefStore;

use crate::{
    common_git_dir_for_git_dir, discover_git_dir, read_repo_config, repository_abbrev,
    repository_object_format, warn_ambiguous_refname_for_object_prefix, worktree_root_for_git_dir,
};

pub(crate) struct RepositoryContext {
    cwd: PathBuf,
    git_dir: PathBuf,
    format: ObjectFormat,
    config: GitConfig,
    objects: FileObjectDatabase,
    refs: FileRefStore,
    worktree_root: OnceLock<PathBuf>,
    abbrev: OnceLock<Option<usize>>,
}

impl RepositoryContext {
    pub(crate) fn discover_current() -> Result<Self> {
        Self::discover(env::current_dir()?)
    }

    pub(crate) fn discover(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let git_dir = discover_git_dir(&cwd)?;
        Self::from_git_dir_and_cwd(git_dir, cwd)
    }

    fn from_git_dir_and_cwd(git_dir: PathBuf, cwd: PathBuf) -> Result<Self> {
        let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
        let format = repository_object_format(&common_git_dir)?;
        let config = read_repo_config(&git_dir)?;
        let objects = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let refs = FileRefStore::new(&git_dir, format);
        Ok(Self {
            cwd,
            git_dir,
            format,
            config,
            objects,
            refs,
            worktree_root: OnceLock::new(),
            abbrev: OnceLock::new(),
        })
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub(crate) fn format(&self) -> ObjectFormat {
        self.format
    }

    pub(crate) fn config(&self) -> &GitConfig {
        &self.config
    }

    pub(crate) fn objects(&self) -> &FileObjectDatabase {
        &self.objects
    }

    pub(crate) fn refs(&self) -> &FileRefStore {
        &self.refs
    }

    pub(crate) fn worktree_root(&self) -> Result<&Path> {
        if let Some(root) = self.worktree_root.get() {
            return Ok(root);
        }
        let root = worktree_root_for_git_dir(&self.git_dir)?;
        let _ = self.worktree_root.set(root);
        Ok(self
            .worktree_root
            .get()
            .expect("repository worktree root should be initialized")
            .as_path())
    }

    pub(crate) fn abbrev(&self) -> Result<Option<usize>> {
        if let Some(abbrev) = self.abbrev.get() {
            return Ok(*abbrev);
        }
        let abbrev = repository_abbrev(&self.git_dir, self.format)?;
        let _ = self.abbrev.set(abbrev);
        Ok(*self
            .abbrev
            .get()
            .expect("repository abbrev should be initialized"))
    }

    pub(crate) fn resolve_revision(&self, rev: &str) -> Result<sley_core::ObjectId> {
        warn_ambiguous_refname_for_object_prefix(&self.git_dir, self.format, rev);
        self.revision_resolver().resolve(rev)
    }

    pub(crate) fn resolve_path(&self, rev: &str, path: &str) -> Result<sley_rev::ResolvedTreePath> {
        warn_ambiguous_refname_for_object_prefix(&self.git_dir, self.format, rev);
        self.revision_resolver().resolve_path(rev, path)
    }

    pub(crate) fn revision_resolver(&self) -> sley_rev::RevisionResolver<'_, FileObjectDatabase> {
        sley_rev::RevisionResolver::new(&self.git_dir, self.format, &self.objects)
    }
}
