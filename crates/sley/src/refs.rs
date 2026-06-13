//! Ref-transaction helpers on top of [`sley_refs::FileRefTransaction`].

use std::collections::HashSet;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use sley_refs::{
    DeleteRef as StoreDeleteRef, DeleteRefReflog, RefDeleteError,
    RefDeletePrecondition as StoreRefDeletePrecondition, RefTarget, RefUpdate, ReflogEntry,
};

use crate::{FullName, GitError, ObjectId, Repository, Result};

/// One ref delete to apply atomically via [`Repository::delete_ref`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRef {
    /// Full ref name (e.g. `refs/heads/main`).
    pub name: FullName,
    /// When set, the ref must currently point at this exact object id.
    pub expected_old: Option<ObjectId>,
    /// Optional richer delete precondition. When present, this takes precedence
    /// over [`DeleteRef::expected_old`].
    pub expected: Option<RefDeleteExpected>,
    /// Optional reflog message appended on successful deletion.
    pub reflog: Option<ReflogMessage>,
    /// Optional reflog committer ident. When omitted, sley builds one from repo
    /// config, matching the older facade behavior.
    pub reflog_committer: Option<Vec<u8>>,
}

/// Compare-and-delete preconditions for [`DeleteRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDeleteExpected {
    /// Any existing direct or symbolic ref may be deleted.
    Any,
    /// The ref's immediate on-disk target must match.
    Immediate(RefTarget),
    /// The ref must immediately point at this object id.
    Direct(ObjectId),
    /// The ref may be symbolic, but its peeled direct target must match.
    Peeled(ObjectId),
}

/// Reflog message for a ref delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogMessage {
    pub message: Vec<u8>,
}

impl ReflogMessage {
    pub fn new(message: impl Into<Vec<u8>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

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

/// One create/update/delete in a mixed ref batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefBatchChange {
    Update(RefChange),
    Delete(DeleteRef),
}

impl From<RefChange> for RefBatchChange {
    fn from(value: RefChange) -> Self {
        Self::Update(value)
    }
}

impl From<DeleteRef> for RefBatchChange {
    fn from(value: DeleteRef) -> Self {
        Self::Delete(value)
    }
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
                Self { ref_name, message }
            }
            other => Self {
                ref_name: "unknown".to_string(),
                message: other.to_string(),
            },
        }
    }

    fn new(ref_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ref_name: ref_name.into(),
            message: message.into(),
        }
    }

    fn from_delete_error(ref_name: String, err: RefDeleteError) -> Self {
        Self {
            ref_name,
            message: err.to_string(),
        }
    }
}

fn extract_ref_name_from_transaction(message: &str) -> Option<String> {
    for prefix in ["expected ref ", "ref ", "could not lock ref "] {
        if let Some(rest) = message.strip_prefix(prefix)
            && let Some(name) = rest.split_whitespace().next()
        {
            return Some(name.to_string());
        }
    }
    None
}

impl Repository {
    /// Delete a direct ref only if the optional old-oid precondition still holds.
    ///
    /// The ref is locked before its current value is read, stale expected values
    /// leave the ref untouched, and packed refs are rewritten through
    /// `packed-refs.lock`.
    pub fn delete_ref(&self, delete: DeleteRef) -> std::result::Result<(), RefDeleteError> {
        let current = if delete.expected.is_some() {
            let name = delete.name.as_str().to_string();
            let current = self
                .references()
                .read_ref(&name)
                .map_err(ref_delete_error_from_git)?;
            self.verify_delete_expected(&name, current.as_ref(), delete.expected.as_ref())?;
            current
        } else {
            None
        };
        let committer = match delete.reflog_committer {
            Some(committer) => committer,
            None => self.delete_ref_committer()?,
        };
        let reflog = match delete.reflog {
            Some(message) => Some(DeleteRefReflog {
                committer,
                message: message.message,
            }),
            None => None,
        };
        if let Some(RefTarget::Symbolic(_)) = current {
            return self
                .references()
                .delete_symbolic_ref(delete.name.as_str())
                .map_err(ref_delete_error_from_git)
                .and_then(|deleted| {
                    if deleted {
                        Ok(())
                    } else {
                        Err(RefDeleteError::NotFound)
                    }
                });
        }
        let expected_old = match (&delete.expected, current.as_ref()) {
            (Some(RefDeleteExpected::Any), _) => delete.expected_old,
            (Some(_), Some(RefTarget::Direct(oid))) => Some(*oid),
            _ => delete.expected_old,
        };
        self.references()
            .delete_ref_checked(StoreDeleteRef {
                name: delete.name.into(),
                expected_old,
                reflog,
            })
            .map(|_| ())
    }

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

    /// Apply a mixed create/update/delete batch through the ref backend.
    ///
    /// The public API is stable for callers that need exact ref plans. Duplicate
    /// names reject before opening the backend transaction; leases are enforced
    /// by the backend while refs are locked.
    pub fn apply_ref_batch(
        &self,
        changes: &[RefBatchChange],
    ) -> std::result::Result<(), RefConflict> {
        let refs = self.references();
        let mut names = HashSet::new();
        let mut tx = refs.transaction();
        for change in changes {
            let name = match change {
                RefBatchChange::Update(change) => change.name.as_str(),
                RefBatchChange::Delete(delete) => delete.name.as_str(),
            };
            if !names.insert(name.to_string()) {
                return Err(RefConflict::new(name, "duplicate ref in batch"));
            }
            match change {
                RefBatchChange::Update(change) => {
                    tx.update(change.clone().into_update());
                }
                RefBatchChange::Delete(delete) => {
                    let reflog = self.store_delete_reflog(delete)?;
                    tx.delete_with_precondition(
                        delete.name.as_str(),
                        store_delete_precondition(delete),
                        reflog,
                    );
                }
            }
        }
        tx.commit().map_err(RefConflict::from_git_error)
    }

    fn delete_ref_committer(&self) -> std::result::Result<Vec<u8>, RefDeleteError> {
        let name = self
            .config_string("user", "name")
            .map_err(ref_delete_error_from_git)?
            .unwrap_or_else(|| "sley".to_string());
        let email = self
            .config_string("user", "email")
            .map_err(ref_delete_error_from_git)?
            .unwrap_or_else(|| "sley@example.invalid".to_string());
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Ok(format!("{name} <{email}> {seconds} +0000").into_bytes())
    }

    fn store_delete_reflog(
        &self,
        delete: &DeleteRef,
    ) -> std::result::Result<Option<DeleteRefReflog>, RefConflict> {
        let Some(message) = delete.reflog.as_ref() else {
            return Ok(None);
        };
        let committer = match delete.reflog_committer.as_ref() {
            Some(committer) => committer.clone(),
            None => self.delete_ref_committer().map_err(|err| {
                RefConflict::from_delete_error(delete.name.as_str().to_string(), err)
            })?,
        };
        Ok(Some(DeleteRefReflog {
            committer,
            message: message.message.clone(),
        }))
    }

    fn verify_delete_expected(
        &self,
        _name: &str,
        current: Option<&RefTarget>,
        expected: Option<&RefDeleteExpected>,
    ) -> std::result::Result<(), RefDeleteError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let Some(current) = current else {
            return Err(RefDeleteError::NotFound);
        };
        match expected {
            RefDeleteExpected::Any => Ok(()),
            RefDeleteExpected::Immediate(target) if current == target => Ok(()),
            RefDeleteExpected::Direct(oid) if current == &RefTarget::Direct(*oid) => Ok(()),
            RefDeleteExpected::Peeled(oid) => {
                let actual = match current {
                    RefTarget::Direct(current) => Some(*current),
                    RefTarget::Symbolic(target) => self
                        .resolve_symbolic(target)
                        .map_err(ref_delete_error_from_git)?,
                };
                if actual == Some(*oid) {
                    Ok(())
                } else {
                    Err(RefDeleteError::ExpectedMismatch {
                        expected: Some(*oid),
                        actual,
                    })
                }
            }
            RefDeleteExpected::Immediate(_) | RefDeleteExpected::Direct(_) => {
                Err(RefDeleteError::ExpectedMismatch {
                    expected: expected.expected_oid(),
                    actual: current.direct_oid(),
                })
            }
        }
    }
}

/// Facade-level ref transaction result type.
pub type RefChangeResult<T> = std::result::Result<T, RefConflict>;

fn ref_delete_error_from_git(err: GitError) -> RefDeleteError {
    RefDeleteError::Io(std::io::Error::other(err.to_string()))
}

fn store_delete_precondition(delete: &DeleteRef) -> StoreRefDeletePrecondition {
    match delete.expected.as_ref() {
        Some(RefDeleteExpected::Any) => StoreRefDeletePrecondition::Any,
        Some(RefDeleteExpected::Immediate(target)) => {
            StoreRefDeletePrecondition::Immediate(target.clone())
        }
        Some(RefDeleteExpected::Direct(oid)) => StoreRefDeletePrecondition::Direct(Some(*oid)),
        Some(RefDeleteExpected::Peeled(oid)) => StoreRefDeletePrecondition::Peeled(*oid),
        None => StoreRefDeletePrecondition::Direct(delete.expected_old),
    }
}

impl RefDeleteExpected {
    fn expected_oid(&self) -> Option<ObjectId> {
        match self {
            Self::Direct(oid) | Self::Peeled(oid) => Some(*oid),
            Self::Immediate(RefTarget::Direct(oid)) => Some(*oid),
            Self::Any | Self::Immediate(RefTarget::Symbolic(_)) => None,
        }
    }
}

trait RefTargetExt {
    fn direct_oid(&self) -> Option<ObjectId>;
}

impl RefTargetExt for RefTarget {
    fn direct_oid(&self) -> Option<ObjectId> {
        match self {
            RefTarget::Direct(oid) => Some(*oid),
            RefTarget::Symbolic(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RefTarget;
    use sley_object::{BString, Commit, EncodedObject, ObjectType, Tree};
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

    fn write_commit(
        repo: &Repository,
        parent: Option<&sley_core::ObjectId>,
    ) -> sley_core::ObjectId {
        let db = repo.objects_mut();
        let blob_oid = db
            .write_object(EncodedObject::new(ObjectType::Blob, b"x\n".to_vec()))
            .expect("blob");
        let tree = Tree {
            entries: vec![sley_object::TreeEntry {
                mode: 0o100644,
                name: BString::from(b"x.txt"),
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

        repo.apply_ref_changes(&[RefChange::new(
            "refs/heads/feature",
            RefTarget::Direct(a.clone()),
        )
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

    #[test]
    fn repository_delete_ref_enforces_expected_and_appends_reflog() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let oid = write_commit(&repo, None);
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(oid)).expect("valid ref name")
        ])
        .expect("create ref");

        repo.delete_ref(DeleteRef {
            name: FullName::new("refs/heads/main").expect("valid ref name"),
            expected_old: Some(oid),
            expected: None,
            reflog: Some(ReflogMessage::new(b"delete main".to_vec())),
            reflog_committer: None,
        })
        .expect("delete ref");

        assert!(
            repo.find_reference("refs/heads/main")
                .expect("lookup")
                .is_none()
        );
        let log = repo
            .references()
            .read_reflog("refs/heads/main")
            .expect("read reflog");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].old_oid, oid);
        assert_eq!(
            log[0].new_oid,
            sley_core::ObjectId::null(repo.object_format())
        );
        assert_eq!(log[0].message, b"delete main");
    }

    #[test]
    fn repository_apply_ref_batch_mixes_update_and_delete() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let a = write_commit(&repo, None);
        let b = write_commit(&repo, Some(&a));
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(a)).expect("valid ref name")
        ])
        .expect("seed main");

        repo.apply_ref_batch(&[
            RefBatchChange::Update(
                RefChange::new("refs/heads/feature", RefTarget::Direct(b)).expect("valid ref name"),
            ),
            RefBatchChange::Delete(DeleteRef {
                name: FullName::new("refs/heads/main").expect("valid ref name"),
                expected_old: Some(a),
                expected: None,
                reflog: None,
                reflog_committer: None,
            }),
        ])
        .expect("mixed batch");

        assert!(
            repo.find_reference("refs/heads/main")
                .expect("lookup main")
                .is_none()
        );
        assert_eq!(
            repo.find_reference("refs/heads/feature")
                .expect("lookup feature")
                .expect("feature")
                .target,
            RefTarget::Direct(b)
        );
    }

    #[test]
    fn repository_apply_ref_batch_stale_delete_leaves_updates_untouched() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let a = write_commit(&repo, None);
        let b = write_commit(&repo, Some(&a));
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(a)).expect("valid ref name"),
            RefChange::new("refs/heads/feature", RefTarget::Direct(a)).expect("valid ref name"),
        ])
        .expect("seed refs");

        let err = repo
            .apply_ref_batch(&[
                RefBatchChange::Update(
                    RefChange::new("refs/heads/feature", RefTarget::Direct(b))
                        .expect("valid ref name"),
                ),
                RefBatchChange::Delete(DeleteRef {
                    name: FullName::new("refs/heads/main").expect("valid ref name"),
                    expected_old: Some(b),
                    expected: None,
                    reflog: None,
                    reflog_committer: None,
                }),
            ])
            .expect_err("stale delete");

        assert_eq!(err.ref_name, "refs/heads/main");
        assert_eq!(
            repo.find_reference("refs/heads/feature")
                .expect("lookup feature")
                .expect("feature")
                .target,
            RefTarget::Direct(a)
        );
        assert_eq!(
            repo.find_reference("refs/heads/main")
                .expect("lookup main")
                .expect("main")
                .target,
            RefTarget::Direct(a)
        );
    }

    #[test]
    fn repository_apply_ref_batch_rejects_duplicate_names() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let a = write_commit(&repo, None);
        let b = write_commit(&repo, Some(&a));

        let err = repo
            .apply_ref_batch(&[
                RefBatchChange::Update(
                    RefChange::new("refs/heads/main", RefTarget::Direct(a))
                        .expect("valid ref name"),
                ),
                RefBatchChange::Update(
                    RefChange::new("refs/heads/main", RefTarget::Direct(b))
                        .expect("valid ref name"),
                ),
            ])
            .expect_err("duplicate");

        assert_eq!(err.ref_name, "refs/heads/main");
    }

    #[test]
    fn repository_apply_ref_batch_deletes_symbolic_ref_with_checked_expectation() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let oid = write_commit(&repo, None);
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(oid)).expect("valid ref name"),
            RefChange::new(
                "refs/alias/main",
                RefTarget::Symbolic("refs/heads/main".into()),
            )
            .expect("valid ref name"),
        ])
        .expect("seed refs");

        repo.apply_ref_batch(&[RefBatchChange::Delete(DeleteRef {
            name: FullName::new("refs/alias/main").expect("valid ref name"),
            expected_old: None,
            expected: Some(RefDeleteExpected::Immediate(RefTarget::Symbolic(
                "refs/heads/main".into(),
            ))),
            reflog: None,
            reflog_committer: None,
        })])
        .expect("delete symbolic ref");

        assert!(
            repo.find_reference("refs/alias/main")
                .expect("lookup alias")
                .is_none()
        );
        assert_eq!(
            repo.find_reference("refs/heads/main")
                .expect("lookup main")
                .expect("main")
                .target,
            RefTarget::Direct(oid)
        );
    }

    #[test]
    fn repository_apply_ref_batch_uses_delete_reflog_committer() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let oid = write_commit(&repo, None);
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(oid)).expect("valid ref name")
        ])
        .expect("seed main");
        let committer = b"Heddle <actor@example.invalid> 7 +0000".to_vec();

        repo.apply_ref_batch(&[RefBatchChange::Delete(DeleteRef {
            name: FullName::new("refs/heads/main").expect("valid ref name"),
            expected_old: Some(oid),
            expected: None,
            reflog: Some(ReflogMessage::new(b"delete main".to_vec())),
            reflog_committer: Some(committer.clone()),
        })])
        .expect("delete ref");

        let log = repo
            .references()
            .read_reflog("refs/heads/main")
            .expect("read reflog");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].committer, committer);
        assert_eq!(log[0].message, b"delete main");
    }

    #[test]
    fn repository_delete_ref_uses_explicit_reflog_committer() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let oid = write_commit(&repo, None);
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(oid)).expect("valid ref name")
        ])
        .expect("seed main");
        let committer = b"Heddle <actor@example.invalid> 7 +0000".to_vec();

        repo.delete_ref(DeleteRef {
            name: FullName::new("refs/heads/main").expect("valid ref name"),
            expected_old: Some(oid),
            expected: None,
            reflog: Some(ReflogMessage::new(b"delete main".to_vec())),
            reflog_committer: Some(committer.clone()),
        })
        .expect("delete ref");

        let log = repo
            .references()
            .read_reflog("refs/heads/main")
            .expect("read reflog");
        assert_eq!(log[0].committer, committer);
    }

    #[test]
    fn repository_delete_ref_checks_symbolic_immediate_target() {
        let temp = TempDir::new();
        let repo = Repository::init(&temp.path).expect("init");
        let oid = write_commit(&repo, None);
        repo.apply_ref_changes(&[
            RefChange::new("refs/heads/main", RefTarget::Direct(oid)).expect("valid ref name"),
            RefChange::new(
                "refs/alias/main",
                RefTarget::Symbolic("refs/heads/main".into()),
            )
            .expect("valid ref name"),
        ])
        .expect("seed refs");

        repo.delete_ref(DeleteRef {
            name: FullName::new("refs/alias/main").expect("valid ref name"),
            expected_old: None,
            expected: Some(RefDeleteExpected::Immediate(RefTarget::Symbolic(
                "refs/heads/main".into(),
            ))),
            reflog: None,
            reflog_committer: None,
        })
        .expect("delete symbolic ref");

        assert!(
            repo.find_reference("refs/alias/main")
                .expect("lookup alias")
                .is_none()
        );
    }
}
