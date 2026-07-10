//! Shared repository handles for command implementations.
//!
//! Commands should be able to say "open the repository" once and then reuse the
//! resulting object database, refs, config, and format. This keeps Git discovery
//! behavior in one place while preserving command-specific parsing and errors.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_core, sley_odb, sley_rev};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sley::GitConfig;
use sley::ObjectDatabase as FileObjectDatabase;
use sley::RefStore as FileRefStore;
use sley::{ObjectFormat, Result};

use crate::{
    common_git_dir_for_git_dir, read_repo_config, repository_abbrev, repository_object_format,
    session, warn_ambiguous_refname_for_object_prefix, worktree_root_for_git_dir,
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
        let cwd = session::cli_session()
            .map(|session| session.cwd)
            .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Self::discover(cwd)
    }

    pub(crate) fn discover(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let git_dir = session::cli_git_dir_from(&cwd)?;
        Self::from_git_dir_and_cwd(git_dir, cwd)
    }

    fn from_git_dir_and_cwd(git_dir: PathBuf, cwd: PathBuf) -> Result<Self> {
        let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
        let format = repository_object_format(&common_git_dir)?;
        let config = read_repo_config(&git_dir)?;
        let refs = FileRefStore::new(&git_dir, format);
        let objects = open_object_database(&git_dir, format)?;
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

/// Open the repository object database with CLI-effective replacement reads.
///
/// The returned database applies replacement refs only to content/header
/// reads. Object writes, enumeration, and raw storage queries remain keyed by
/// the caller-provided object id. The policy is resolved here from the parsed
/// global CLI session, effective config (including `-c`), and repository refs;
/// the ODB itself does not inspect process state.
pub(crate) fn open_object_database(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<FileObjectDatabase> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config = read_repo_config(git_dir)?;
    let refs = FileRefStore::new(git_dir, format);
    let replace_objects = session::cli_session()
        .map(|session| session.replace_objects())
        .unwrap_or(true)
        && config
            .get_bool("core", None, "useReplaceRefs")
            .unwrap_or(true);
    let replacements = if replace_objects {
        repository_object_replacements(&refs, format)?
    } else {
        sley_odb::ObjectReplacements::default()
    };
    Ok(FileObjectDatabase::from_git_dir(&common_git_dir, format).with_replacements(replacements))
}

pub(crate) fn warn_graft_file_deprecated(git_dir: &Path, config: &GitConfig) {
    if config
        .get_bool("advice", None, "graftFileDeprecated")
        .unwrap_or(true)
        && git_dir.join("info").join("grafts").exists()
    {
        eprintln!("hint: Support for <GIT_DIR>/info/grafts is deprecated");
        eprintln!("hint: and will be removed in a future Git version.");
        eprintln!("hint: ");
        eprintln!("hint: Please use \"git replace --convert-graft-file\"");
        eprintln!("hint: to convert the grafts into replace refs.");
        eprintln!("hint: ");
        eprintln!("hint: Turn this message off by running");
        eprintln!("hint: \"git config set advice.graftFileDeprecated false\"");
    }
}

fn repository_object_replacements(
    refs: &FileRefStore,
    format: ObjectFormat,
) -> Result<sley_odb::ObjectReplacements> {
    let mut replacements = Vec::new();
    for reference in refs.list_refs()? {
        let Some(source) = reference.name.strip_prefix("refs/replace/") else {
            continue;
        };
        let Ok(source) = sley_core::ObjectId::from_hex(format, source) else {
            continue;
        };
        if let sley::ReferenceTarget::Direct(target) = reference.target {
            replacements.push((source, target));
        }
    }
    Ok(sley_odb::ObjectReplacements::new(replacements))
}
