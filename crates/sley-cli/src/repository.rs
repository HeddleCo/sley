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

use sley::ObjectDatabase as FileObjectDatabase;
use sley::RefStore as FileRefStore;
use sley::{GitConfig, OpenOptions, Repository};
use sley::{ObjectFormat, Result};

use crate::{
    common_git_dir_for_git_dir, read_repo_config, repository_abbrev, repository_object_format,
    session, warn_ambiguous_refname_for_object_prefix, worktree_root_for_git_dir,
};

pub(crate) struct RepositoryContext {
    cwd: PathBuf,
    repository: Repository,
    config: GitConfig,
    refs: FileRefStore,
    worktree_root: OnceLock<PathBuf>,
    abbrev: OnceLock<Option<usize>>,
}

impl RepositoryContext {
    /// Open the invocation repository without consulting compatibility globals.
    pub(crate) fn from_session(cli_session: &session::CliSession) -> Result<Self> {
        Self::from_git_dir_and_cwd(
            cli_session.git_dir()?,
            cli_session.cwd().to_path_buf(),
            cli_session.replace_objects(),
        )
    }

    pub(crate) fn discover_current() -> Result<Self> {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::discover(cwd)
    }

    pub(crate) fn discover(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let git_dir = session::cli_git_dir_from(&cwd)?;
        let replace_objects = session::cli_session()
            .map(|session| session.replace_objects())
            .unwrap_or(true);
        Self::from_git_dir_and_cwd(git_dir, cwd, replace_objects)
    }

    fn from_git_dir_and_cwd(git_dir: PathBuf, cwd: PathBuf, replace_objects: bool) -> Result<Self> {
        let config = read_repo_config(&git_dir)?;
        let use_replace_refs = config
            .get_bool("core", None, "useReplaceRefs")
            .unwrap_or(true);
        let repository = Repository::open_with(
            &git_dir,
            OpenOptions::new()
                .exact_path(true)
                .replace_objects(replace_objects)
                .effective_use_replace_refs(use_replace_refs),
        )?;
        let refs = repository.references();
        Ok(Self {
            cwd,
            repository,
            config,
            refs,
            worktree_root: OnceLock::new(),
            abbrev: OnceLock::new(),
        })
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn repository(&self) -> &Repository {
        &self.repository
    }

    pub(crate) fn git_dir(&self) -> &Path {
        self.repository.git_dir()
    }

    pub(crate) fn format(&self) -> ObjectFormat {
        self.repository.object_format()
    }

    pub(crate) fn config(&self) -> &GitConfig {
        &self.config
    }

    pub(crate) fn objects(&self) -> &FileObjectDatabase {
        self.repository.object_database()
    }

    pub(crate) fn refs(&self) -> &FileRefStore {
        &self.refs
    }

    pub(crate) fn worktree_root(&self) -> Result<&Path> {
        if let Some(root) = self.worktree_root.get() {
            return Ok(root);
        }
        let root = worktree_root_for_git_dir(self.repository.git_dir())?;
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
        let abbrev = repository_abbrev(self.repository.git_dir(), self.repository.object_format())?;
        let _ = self.abbrev.set(abbrev);
        Ok(*self
            .abbrev
            .get()
            .expect("repository abbrev should be initialized"))
    }

    pub(crate) fn resolve_revision(&self, rev: &str) -> Result<sley_core::ObjectId> {
        warn_ambiguous_refname_for_object_prefix(
            self.repository.git_dir(),
            self.repository.object_format(),
            rev,
        );
        self.revision_resolver().resolve(rev)
    }

    pub(crate) fn resolve_path(&self, rev: &str, path: &str) -> Result<sley_rev::ResolvedTreePath> {
        warn_ambiguous_refname_for_object_prefix(
            self.repository.git_dir(),
            self.repository.object_format(),
            rev,
        );
        self.revision_resolver().resolve_path(rev, path)
    }

    pub(crate) fn revision_resolver(&self) -> sley_rev::RevisionResolver<'_, FileObjectDatabase> {
        sley_rev::RevisionResolver::new(
            self.repository.git_dir(),
            self.repository.object_format(),
            self.repository.object_database(),
        )
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
