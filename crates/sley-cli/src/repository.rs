//! Shared repository handles for command implementations.
//!
//! Commands should be able to say "open the repository" once and then reuse the
//! resulting object database, refs, config, and format. This keeps Git discovery
//! behavior in one place while preserving command-specific parsing and errors.
#![allow(clippy::expect_used)]

use sley::plumbing::{sley_core, sley_odb, sley_rev};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::sley_worktree;
use sley::ObjectDatabase as FileObjectDatabase;
use sley::RefStore as FileRefStore;
use sley::{GitConfig, Repository, ResolvedRepositoryOpen};
use sley::{ObjectFormat, Result};

use crate::{
    common_git_dir_for_git_dir, read_repo_config, repository_abbrev_from_config, session,
};

/// Object-database behavior required by a command's invocation snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectAccess {
    /// Repository layout/config only; no object reads are performed.
    None,
    /// Raw object writes. Replacement refs never participate in writes.
    WriteOnly,
    /// Object reads with the invocation's replacement policy.
    ReadWithReplacements,
}

/// Purpose-specific worktree semantics for a repository snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreePolicy {
    /// Normal command setup semantics.
    Command,
    /// Hash-object attribute lookup semantics, based on physical layout.
    HashAttributes,
}

pub(crate) struct RepositoryContext {
    cwd: PathBuf,
    snapshot: Arc<session::InvocationRepositorySnapshot>,
    repository: Repository,
    refs: FileRefStore,
    pathspec_magic: sley_worktree::PathspecMatchMagic,
    worktree_root: Option<PathBuf>,
    abbrev: OnceLock<Option<usize>>,
}

impl RepositoryContext {
    /// Open the invocation repository without consulting compatibility globals.
    pub(crate) fn from_session(cli_session: &session::CliSession) -> Result<Self> {
        Self::from_session_with_access(
            cli_session,
            ObjectAccess::ReadWithReplacements,
            WorktreePolicy::Command,
        )
    }

    /// Open one resolved repository/config/worktree snapshot for the command.
    pub(crate) fn from_session_with_access(
        cli_session: &session::CliSession,
        access: ObjectAccess,
        worktree_policy: WorktreePolicy,
    ) -> Result<Self> {
        let snapshot = cli_session.repository_snapshot()?;
        let worktree_root = cli_session.optional_worktree_from_config(
            &snapshot.git_dir,
            &snapshot.setup_config,
            &snapshot.config,
            snapshot.linked_worktree,
            worktree_policy,
        )?;
        let use_replace_refs = snapshot
            .config
            .get_bool("core", None, "useReplaceRefs")
            .unwrap_or(true);
        let replacement_reads =
            access == ObjectAccess::ReadWithReplacements && cli_session.replace_objects();
        let repository = Repository::open_resolved(
            ResolvedRepositoryOpen {
                git_dir: snapshot.git_dir.clone(),
                common_dir: snapshot.common_dir.clone(),
                work_tree: worktree_root.clone(),
                format: snapshot.format,
                use_replace_refs,
            },
            replacement_reads,
        )?;
        let refs = repository.references();
        Ok(Self {
            cwd: cli_session.cwd().to_path_buf(),
            snapshot,
            repository,
            refs,
            pathspec_magic: crate::effective_pathspec_flags(cli_session),
            worktree_root,
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
        &self.snapshot.git_dir
    }

    pub(crate) fn common_git_dir(&self) -> &Path {
        &self.snapshot.common_dir
    }

    pub(crate) fn format(&self) -> ObjectFormat {
        self.snapshot.format
    }

    pub(crate) fn config(&self) -> &GitConfig {
        &self.snapshot.config
    }

    pub(crate) fn objects(&self) -> &FileObjectDatabase {
        self.repository().object_database()
    }

    pub(crate) fn refs(&self) -> &FileRefStore {
        &self.refs
    }

    pub(crate) fn pathspec_magic(&self) -> sley_worktree::PathspecMatchMagic {
        self.pathspec_magic
    }

    pub(crate) fn worktree_root(&self) -> Result<&Path> {
        self.worktree_root.as_deref().ok_or_else(|| {
            sley::GitError::Unsupported("command requires a non-bare worktree".into())
        })
    }

    pub(crate) fn abbrev(&self) -> Result<Option<usize>> {
        if let Some(abbrev) = self.abbrev.get() {
            return Ok(*abbrev);
        }
        let abbrev = repository_abbrev_from_config(self.git_dir(), self.format(), self.config())?;
        let _ = self.abbrev.set(abbrev);
        Ok(*self
            .abbrev
            .get()
            .expect("repository abbrev should be initialized"))
    }

    pub(crate) fn resolve_revision(&self, rev: &str) -> Result<sley_core::ObjectId> {
        // The prefix probe reuses this context's shared object database:
        // enumeration ignores replacement policy, so the answer matches a
        // plain open without paying one per resolution.
        sley_rev::warn_ambiguous_refname_with_sink(
            self.git_dir(),
            self.format(),
            rev,
            Some(self.objects()),
            sley_rev::AmbiguousRefnameWarning::Stderr,
        );
        self.revision_resolver().resolve(rev)
    }

    pub(crate) fn resolve_path(&self, rev: &str, path: &str) -> Result<sley_rev::ResolvedTreePath> {
        sley_rev::warn_ambiguous_refname_with_sink(
            self.git_dir(),
            self.format(),
            rev,
            Some(self.objects()),
            sley_rev::AmbiguousRefnameWarning::Stderr,
        );
        self.revision_resolver().resolve_path(rev, path)
    }

    pub(crate) fn revision_resolver(&self) -> sley_rev::RevisionResolver<'_, FileObjectDatabase> {
        sley_rev::RevisionResolver::new(self.git_dir(), self.format(), self.objects())
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
    replace_objects: bool,
) -> Result<FileObjectDatabase> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let config = read_repo_config(git_dir)?;
    let refs = FileRefStore::new(git_dir, format);
    let replace_objects = replace_objects
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
    // Prefer the replace namespace only so opening an object database does not
    // emit "ignoring broken ref" warnings for unrelated refs (t6301).
    for reference in refs.list_refs_with_prefix("refs/replace/")? {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn abbrev_uses_the_invocation_config_snapshot() {
        let root = unique_temp_dir("repository-context-abbrev-snapshot");
        let initialized = Repository::init(&root).expect("initialize repository");
        let config_path = initialized.git_dir().join("config");
        let mut config = fs::read_to_string(&config_path).expect("read initial config");
        config.push_str("\n[core]\n\tabbrev = 12\n");
        fs::write(&config_path, &config).expect("set initial abbrev");

        let cli_session = session::CliSession::from_parsed_globals(
            root.clone(),
            Some(initialized.git_dir().to_path_buf()),
            None,
            None,
            false,
            true,
            true,
            crate::PathspecFlags::default(),
        );
        let context = RepositoryContext::from_session_with_access(
            &cli_session,
            ObjectAccess::None,
            WorktreePolicy::Command,
        )
        .expect("capture repository context");

        config.push_str("\n[core]\n\tabbrev = 4\n");
        fs::write(&config_path, config).expect("mutate config after snapshot");
        assert_eq!(context.abbrev().expect("read captured abbrev"), Some(12));

        fs::remove_dir_all(root).expect("remove fixture");
    }
}
