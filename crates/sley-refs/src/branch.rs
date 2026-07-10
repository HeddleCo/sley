//! Typed branch reference operations.
//!
//! This module contains repository-independent branch selection and transfer
//! semantics. Porcelain callers remain responsible for argv parsing,
//! worktree/config checks, and exact user-facing diagnostics.

use crate::{FileRefStore, Ref, RefTarget, refname_pattern_matches_case};
use sley_core::GitError;
use std::error::Error;
use std::fmt;

/// Branch namespaces included by a list query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchListScope {
    /// `refs/heads/*`.
    Local,
    /// `refs/remotes/*`.
    Remote,
    /// Both local and remote-tracking branches.
    All,
}

/// Engine options for selecting branch refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchListOptions {
    /// Branch namespaces to inspect.
    pub scope: BranchListScope,
    /// Wildcard patterns matched against the namespace-relative branch name.
    pub patterns: Vec<String>,
    /// Match patterns without ASCII case sensitivity.
    pub ignore_case: bool,
}

impl BranchListOptions {
    /// Select every branch in `scope`.
    #[must_use]
    pub const fn all_in(scope: BranchListScope) -> Self {
        Self {
            scope,
            patterns: Vec::new(),
            ignore_case: false,
        }
    }
}

/// Selected branch refs plus the attached local branch, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchListOutcome {
    /// Matching references in canonical refname order.
    pub refs: Vec<Ref>,
    /// Full `refs/heads/*` name attached to `HEAD`.
    pub current_branch_ref: Option<String>,
}

/// A branch rename or copy operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchTransferKind {
    /// Move the source ref and its reflog.
    Move,
    /// Copy the source ref and its reflog.
    Copy,
}

/// Options for a branch transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTransferOptions {
    /// Transfer behavior.
    pub kind: BranchTransferKind,
    /// Namespace-relative source branch name.
    pub source: String,
    /// Namespace-relative destination branch name.
    pub destination: String,
    /// Permit replacing an existing destination.
    pub force: bool,
    /// Reflog identity for the synthesized rename/copy entry.
    pub committer: Vec<u8>,
}

/// Result of a successful branch transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTransferOutcome {
    /// Full source ref name.
    pub source_ref: String,
    /// Full destination ref name.
    pub destination_ref: String,
    /// Immediate target copied or moved to the destination.
    pub target: RefTarget,
    /// Whether the source ref was removed.
    pub source_deleted: bool,
}

/// Typed branch-operation failure.
#[derive(Debug)]
pub enum BranchOperationError {
    /// The ref backend rejected the operation.
    Backend(GitError),
}

impl BranchOperationError {
    /// Recover the plumbing error for byte-identical porcelain diagnostics.
    #[must_use]
    pub fn into_git_error(self) -> GitError {
        match self {
            Self::Backend(error) => error,
        }
    }
}

impl fmt::Display for BranchOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "branch reference operation failed: {error}"),
        }
    }
}

impl Error for BranchOperationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
        }
    }
}

impl From<GitError> for BranchOperationError {
    fn from(error: GitError) -> Self {
        Self::Backend(error)
    }
}

/// Select branch refs without applying any CLI rendering policy.
pub fn list_branches(
    store: &FileRefStore,
    options: &BranchListOptions,
) -> Result<BranchListOutcome, BranchOperationError> {
    let mut refs = match options.scope {
        BranchListScope::Local => store.list_refs_with_prefix("refs/heads/")?,
        BranchListScope::Remote => store.list_refs_with_prefix("refs/remotes/")?,
        BranchListScope::All => {
            let mut refs = store.list_refs_with_prefix("refs/heads/")?;
            refs.extend(store.list_refs_with_prefix("refs/remotes/")?);
            refs
        }
    };
    refs.retain(|reference| {
        let Some(short_name) = branch_short_name(&reference.name, options.scope) else {
            return false;
        };
        options.patterns.is_empty()
            || options.patterns.iter().any(|pattern| {
                refname_pattern_matches_case(pattern, short_name, options.ignore_case)
            })
    });
    refs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(BranchListOutcome {
        refs,
        current_branch_ref: store.current_branch_ref()?,
    })
}

/// Move or copy one branch through the ref backend.
pub fn transfer_branch(
    store: &FileRefStore,
    options: BranchTransferOptions,
) -> Result<BranchTransferOutcome, BranchOperationError> {
    let source_ref = format!("refs/heads/{}", options.source);
    let destination_ref = format!("refs/heads/{}", options.destination);
    let target = store
        .read_ref(&source_ref)?
        .ok_or_else(|| GitError::reference_not_found(format!("branch {}", options.source)))?;
    match options.kind {
        BranchTransferKind::Move => store.move_branch(
            &options.source,
            &options.destination,
            options.force,
            options.committer,
        )?,
        BranchTransferKind::Copy => store.copy_branch(
            &options.source,
            &options.destination,
            options.force,
            options.committer,
        )?,
    }
    Ok(BranchTransferOutcome {
        source_ref,
        destination_ref,
        target,
        source_deleted: matches!(options.kind, BranchTransferKind::Move),
    })
}

fn branch_short_name(name: &str, scope: BranchListScope) -> Option<&str> {
    if matches!(scope, BranchListScope::Local | BranchListScope::All)
        && let Some(name) = name.strip_prefix("refs/heads/")
    {
        return Some(name);
    }
    if matches!(scope, BranchListScope::Remote | BranchListScope::All) {
        return name.strip_prefix("refs/remotes/");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RefUpdate;
    use sley_core::{ObjectFormat, ObjectId};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (std::path::PathBuf, FileRefStore, ObjectId) {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("sley-refs-branch-{}-{counter}", std::process::id()));
        fs::create_dir_all(root.join("refs/heads"))
            .expect("branch fixture should create heads directory");
        fs::create_dir_all(root.join("refs/remotes/origin"))
            .expect("branch fixture should create remote directory");
        let store = FileRefStore::new(&root, ObjectFormat::Sha1);
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .expect("fixture object id should parse");
        let mut transaction = store.transaction();
        for name in [
            "refs/heads/main",
            "refs/heads/topic/One",
            "refs/remotes/origin/main",
        ] {
            transaction.update(RefUpdate {
                name: name.into(),
                expected: None,
                new: RefTarget::Direct(oid),
                reflog: None,
            });
        }
        transaction.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        transaction
            .commit()
            .expect("branch fixture refs should commit");
        (root, store, oid)
    }

    #[test]
    fn list_options_select_scope_patterns_and_current_branch() {
        let (root, store, _) = fixture();
        let outcome = list_branches(
            &store,
            &BranchListOptions {
                scope: BranchListScope::All,
                patterns: vec!["*one".into()],
                ignore_case: true,
            },
        )
        .expect("branch query should succeed");
        assert_eq!(outcome.refs.len(), 1);
        assert_eq!(outcome.refs[0].name, "refs/heads/topic/One");
        assert_eq!(
            outcome.current_branch_ref.as_deref(),
            Some("refs/heads/main")
        );
        fs::remove_dir_all(root).expect("branch fixture should be removed");
    }

    #[test]
    fn transfer_returns_typed_copy_and_move_outcomes() {
        let (root, store, oid) = fixture();
        let copied = transfer_branch(
            &store,
            BranchTransferOptions {
                kind: BranchTransferKind::Copy,
                source: "main".into(),
                destination: "copy".into(),
                force: false,
                committer: b"A U Thor <a@example.com> 1 +0000".to_vec(),
            },
        )
        .expect("branch copy should succeed");
        assert_eq!(copied.target, RefTarget::Direct(oid));
        assert!(!copied.source_deleted);

        let moved = transfer_branch(
            &store,
            BranchTransferOptions {
                kind: BranchTransferKind::Move,
                source: "copy".into(),
                destination: "moved".into(),
                force: false,
                committer: b"A U Thor <a@example.com> 2 +0000".to_vec(),
            },
        )
        .expect("branch move should succeed");
        assert!(moved.source_deleted);
        assert!(
            store
                .read_ref("refs/heads/copy")
                .expect("copy ref read should succeed")
                .is_none()
        );
        assert_eq!(
            store
                .read_ref("refs/heads/moved")
                .expect("moved ref read should succeed"),
            Some(RefTarget::Direct(oid))
        );
        fs::remove_dir_all(root).expect("branch fixture should be removed");
    }

    #[test]
    fn operation_errors_retain_backend_source() {
        let (root, store, _) = fixture();
        let error = transfer_branch(
            &store,
            BranchTransferOptions {
                kind: BranchTransferKind::Move,
                source: "missing".into(),
                destination: "new".into(),
                force: false,
                committer: Vec::new(),
            },
        )
        .expect_err("missing source should fail");
        assert!(error.source().is_some());
        fs::remove_dir_all(root).expect("branch fixture should be removed");
    }
}
