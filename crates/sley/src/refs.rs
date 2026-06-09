//! Ref-transaction helpers on top of [`sley_refs::FileRefTransaction`].

use std::fmt;

use sley_refs::{RefUpdate, ReflogEntry, RefTarget};

use crate::{FullName, GitError, Repository, Result};

/// One ref update to apply atomically via [`Repository::apply_ref_changes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: FullName,
    /// New target after the update.
    pub new: RefTarget,
    /// When set, the ref must currently match this target (compare-and-swap).
    pub expected: Option<RefTarget>,
    /// Optional reflog entry appended on success.
    pub reflog: Option<ReflogEntry>,
}

impl RefChange {
    /// Build a ref update with no compare-and-swap precondition or reflog entry.
    pub fn new(
        name: impl TryInto<FullName, Error = GitError>,
        new: RefTarget,
    ) -> Result<RefChange> {
        Ok(Self {
            name: name.try_into()?,
            new,
            expected: None,
            reflog: None,
        })
    }

    /// Convert into the plumbing [`RefUpdate`] used by [`FileRefStore::transaction`].
    pub fn into_update(self) -> RefUpdate {
        RefUpdate {
            name: self.name.into(),
            expected: self.expected,
            new: self.new,
            reflog: self.reflog,
        }
    }
}

/// A ref update was rejected because a compare-and-swap precondition failed or
/// the ref store could not commit the transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefConflict {
    /// The ref that could not be updated.
    pub ref_name: String,
    /// Human-readable reason (mirrors the underlying transaction error).
    pub message: String,
}

impl fmt::Display for RefConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ref conflict on {}: {}", self.ref_name, self.message)
    }
}

impl std::error::Error for RefConflict {}

impl RefConflict {
    fn from_git_error(err: GitError) -> Self {
        match err {
            GitError::Transaction(message) => {
                let ref_name = extract_ref_name_from_transaction(&message)
                    .unwrap_or_else(|| "unknown".to_string());
                Self {
                    ref_name,
                    message,
                }
            }
            other => Self {
                ref_name: "unknown".to_string(),
                message: other.to_string(),
            },
        }
    }
}

fn extract_ref_name_from_transaction(message: &str) -> Option<String> {
    for prefix in ["expected ref ", "ref ", "could not lock ref "] {
        if let Some(rest) = message.strip_prefix(prefix)
            && let Some(name) = rest.split_whitespace().next() {
                return Some(name.to_string());
            }
    }
    None
}

impl Repository {
    /// Apply `changes` atomically via the on-disk ref transaction backend.
    ///
    /// All updates succeed together or none take effect. A failed
    /// compare-and-swap returns [`RefConflict`] rather than a generic
    /// [`GitError::Transaction`].
    pub fn apply_ref_changes(&self, changes: &[RefChange]) -> std::result::Result<(), RefConflict> {
        let refs = self.references();
        let mut tx = refs.transaction();
        for change in changes {
            tx.update(change.clone().into_update());
        }
        tx.commit().map_err(RefConflict::from_git_error)
    }
}

/// Facade-level ref transaction result type.
pub type RefChangeResult<T> = std::result::Result<T, RefConflict>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RefTarget;
    use sley_object::{Commit, EncodedObject, ObjectType, Tree};
    use sley_odb::ObjectWriter;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sley-ref-change-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_commit(repo: &Repository, parent: Option<&sley_core::ObjectId>) -> sley_core::ObjectId {
        let mut db = repo.objects_mut();
        let blob_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"x\n".to_vec()))
            .expect("blob");
        let tree = Tree {
            entries: vec![sley_object::TreeEntry {
                mode: 0o100644,
                name: b"x.txt".to_vec(),
                oid: blob_oid,
            }],
        };
        let tree_oid = db
            .write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
            .expect("tree");
        let commit = Commit {
            tree: tree_oid,
            parents: parent.into_iter().cloned().collect(),
            author: b"T <t@e.com> 1 +0000".to_vec(),
            committer: b"T <t@e.com> 1 +0000".to_vec(),
            encoding: None,
            message: b"c\n".to_vec(),
        };
        db.write_object(EncodedObject::new(ObjectType::Commit, commit.write()))
            .expect("commit")
    }

    #[test]
    fn apply_ref_changes_updates_atomically() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let a = write_commit(&repo, None);
        repo.references()
            .create_branch("main", a.clone(), Vec::new(), Vec::new())
            .expect("branch");
        let b = write_commit(&repo, Some(&a));

        repo.apply_ref_changes(&[RefChange::new("refs/heads/feature", RefTarget::Direct(a.clone()))
            .expect("valid ref name")])
        .expect("create branch");

        let feature = repo
            .find_reference("refs/heads/feature")
            .expect("lookup")
            .expect("exists");
        assert_eq!(feature.target, RefTarget::Direct(a.clone()));

        repo.apply_ref_changes(&[RefChange {
            name: FullName::new("refs/heads/main").expect("valid ref name"),
            new: RefTarget::Direct(b.clone()),
            expected: Some(RefTarget::Direct(a.clone())),
            reflog: None,
        }])
        .expect("matching expected succeeds");

        let stale = write_commit(&repo, Some(&b));
        let err = repo
            .apply_ref_changes(&[RefChange {
                name: FullName::new("refs/heads/main").expect("valid ref name"),
                new: RefTarget::Direct(stale),
                expected: Some(RefTarget::Direct(a)),
                reflog: None,
            }])
            .expect_err("stale expected");
        assert_eq!(err.ref_name, "refs/heads/main");
    }
}