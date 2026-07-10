//! Typed planning for linked-worktree administration.
//!
//! The CLI owns argv parsing, Git-compatible diagnostics, and filesystem
//! execution. This module owns the reusable repository snapshot and the state
//! transitions for `worktree add/list/move/remove/lock/unlock/prune/repair`.

use sley_core::Result;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktreeAdmin {
    pub admin_dir: PathBuf,
    pub admin_name: String,
    pub path: PathBuf,
    pub prunable_reason: Option<String>,
    pub locked_reason: Option<String>,
}

/// A single, sorted scan of `$GIT_COMMON_DIR/worktrees`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeAdminSnapshot {
    linked: Vec<LinkedWorktreeAdmin>,
}

impl WorktreeAdminSnapshot {
    pub fn scan(common_git_dir: &Path) -> Result<Self> {
        let worktrees_dir = common_git_dir.join("worktrees");
        let Ok(entries) = fs::read_dir(worktrees_dir) else {
            return Ok(Self::default());
        };
        let mut linked = Vec::new();
        for entry in entries {
            let admin_dir = entry?.path();
            if !admin_dir.is_dir() {
                continue;
            }
            if let Some(admin) = read_linked_worktree_admin(&admin_dir)? {
                linked.push(admin);
            }
        }
        linked.sort_by(|left, right| left.admin_dir.cmp(&right.admin_dir));
        Ok(Self { linked })
    }

    pub fn linked(&self) -> &[LinkedWorktreeAdmin] {
        &self.linked
    }

    /// Find a linked worktree by canonical path when possible, falling back to
    /// lexical normalization for missing worktrees.
    pub fn find_path(&self, target: &Path) -> Option<&LinkedWorktreeAdmin> {
        let canonical_target = fs::canonicalize(target).ok();
        self.linked.iter().find(|admin| {
            canonical_target
                .as_ref()
                .is_some_and(|target| fs::canonicalize(&admin.path).ok().as_ref() == Some(target))
                || normalize_lexical_path(target) == normalize_lexical_path(&admin.path)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeAdminOperation {
    Add,
    List,
    Move,
    Remove,
    Lock,
    Unlock,
    Prune,
    Repair,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeAdminOutcome {
    Added { path: PathBuf },
    Listed { count: usize },
    Moved { from: PathBuf, to: PathBuf },
    Removed { path: PathBuf },
    Locked { path: PathBuf },
    Unlocked { path: PathBuf },
    Pruned,
    Repaired,
}

impl WorktreeAdminOutcome {
    pub fn operation(&self) -> WorktreeAdminOperation {
        match self {
            Self::Added { .. } => WorktreeAdminOperation::Add,
            Self::Listed { .. } => WorktreeAdminOperation::List,
            Self::Moved { .. } => WorktreeAdminOperation::Move,
            Self::Removed { .. } => WorktreeAdminOperation::Remove,
            Self::Locked { .. } => WorktreeAdminOperation::Lock,
            Self::Unlocked { .. } => WorktreeAdminOperation::Unlock,
            Self::Pruned => WorktreeAdminOperation::Prune,
            Self::Repaired => WorktreeAdminOperation::Repair,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddWorktreePlan {
    pub path: PathBuf,
    pub force: usize,
    /// Contents for the linked admin's `locked` file, when requested.
    pub lock_contents: Option<Vec<u8>>,
}

pub fn plan_add(
    path: PathBuf,
    force: usize,
    lock: bool,
    lock_reason: Option<&str>,
) -> AddWorktreePlan {
    let lock_contents = lock.then(|| {
        lock_reason
            .map(|reason| format!("{reason}\n").into_bytes())
            .unwrap_or_default()
    });
    AddWorktreePlan {
        path,
        force,
        lock_contents,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddRegistrationPlan {
    Available,
    Replace { admin_dir: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddRegistrationError {
    pub locked: bool,
    pub required_force: usize,
}

/// Plan registration when the destination is already present in the linked
/// admin snapshot. Missing locked worktrees require two force occurrences;
/// missing unlocked worktrees require one.
pub fn plan_add_registration(
    snapshot: &WorktreeAdminSnapshot,
    target: &Path,
    force: usize,
) -> std::result::Result<AddRegistrationPlan, AddRegistrationError> {
    let Some(admin) = snapshot.find_path(target) else {
        return Ok(AddRegistrationPlan::Available);
    };
    let locked = admin.locked_reason.is_some();
    let required_force = if locked { 2 } else { 1 };
    if force >= required_force {
        return Ok(AddRegistrationPlan::Replace {
            admin_dir: admin.admin_dir.clone(),
        });
    }
    Err(AddRegistrationError {
        locked,
        required_force,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListWorktreesPlan {
    pub linked: Vec<LinkedWorktreeAdmin>,
    pub include_prunable: bool,
}

pub fn plan_list(snapshot: &WorktreeAdminSnapshot, include_prunable: bool) -> ListWorktreesPlan {
    ListWorktreesPlan {
        linked: snapshot.linked.clone(),
        include_prunable,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneWorktreesPlan {
    pub dry_run: bool,
    pub verbose: bool,
    pub expire: i64,
}

pub fn plan_prune(dry_run: bool, verbose: bool, expire: i64) -> PruneWorktreesPlan {
    PruneWorktreesPlan {
        dry_run,
        verbose,
        expire,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneKeptWorktree {
    pub path: PathBuf,
    pub admin_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneAdminDecision {
    Prune(String),
    Keep { gitdir: PathBuf },
    Skip,
}

/// Inspect one linked-worktree admin entry and decide whether Git's expiry
/// rules prune it. Execution and diagnostics remain with the caller.
pub fn plan_prune_admin(admin_dir: &Path, expire: i64) -> Result<PruneAdminDecision> {
    if !admin_dir.is_dir() {
        return Ok(PruneAdminDecision::Prune(
            "not a valid directory".to_string(),
        ));
    }
    if admin_dir.join("locked").exists() {
        return Ok(PruneAdminDecision::Skip);
    }
    let gitdir_file = admin_dir.join("gitdir");
    let metadata = match fs::metadata(&gitdir_file) {
        Ok(metadata) => metadata,
        Err(_) => {
            return Ok(PruneAdminDecision::Prune(
                "gitdir file does not exist".to_string(),
            ));
        }
    };
    let mut path = match fs::read_to_string(&gitdir_file) {
        Ok(path) => path,
        Err(err) => {
            return Ok(PruneAdminDecision::Prune(format!(
                "unable to read gitdir file ({err})"
            )));
        }
    };
    let expected = metadata.len() as usize;
    if path.len() != expected {
        return Ok(PruneAdminDecision::Prune(format!(
            "short read (expected {expected} bytes, read {})",
            path.len()
        )));
    }
    while path.ends_with(['\n', '\r']) {
        path.pop();
    }
    if path.is_empty() {
        return Ok(PruneAdminDecision::Prune("invalid gitdir file".to_string()));
    }
    let gitdir = resolve_admin_path_forgiving(admin_dir, &path);
    if !gitdir.exists() {
        let index = admin_dir.join("index");
        let expired = fs::metadata(index)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64 <= expire)
            .unwrap_or(true);
        if expired {
            return Ok(PruneAdminDecision::Prune(
                "gitdir file points to non-existent location".to_string(),
            ));
        }
    }
    Ok(PruneAdminDecision::Keep { gitdir })
}

/// Return duplicate linked admin names in deterministic removal order. The
/// main worktree is represented by a kept record with `admin_name: None` and
/// wins ties over linked entries.
pub fn plan_duplicate_prunes(mut kept: Vec<PruneKeptWorktree>) -> Vec<String> {
    kept.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| match (&left.admin_name, &right.admin_name) {
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(left), Some(right)) => left.cmp(right),
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
    kept.windows(2)
        .filter_map(|pair| {
            (pair[0].path == pair[1].path)
                .then(|| pair[1].admin_name.clone())
                .flatten()
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairWorktreesPlan {
    pub targets: Vec<(PathBuf, Option<String>)>,
    pub relative_paths: bool,
    pub repair_registered: bool,
}

pub fn plan_repair(
    targets: Vec<(PathBuf, Option<String>)>,
    relative_paths: bool,
) -> RepairWorktreesPlan {
    RepairWorktreesPlan {
        targets,
        relative_paths,
        repair_registered: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockWorktreePlan {
    pub lock_file: PathBuf,
    pub contents: Vec<u8>,
    pub worktree_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockWorktreeError {
    AlreadyLocked { reason: String },
}

pub fn plan_lock(
    admin: &LinkedWorktreeAdmin,
    reason: Option<&str>,
) -> std::result::Result<LockWorktreePlan, LockWorktreeError> {
    if let Some(reason) = admin.locked_reason.as_ref() {
        return Err(LockWorktreeError::AlreadyLocked {
            reason: reason.clone(),
        });
    }
    Ok(LockWorktreePlan {
        lock_file: admin.admin_dir.join("locked"),
        contents: reason
            .map(|reason| format!("{reason}\n").into_bytes())
            .unwrap_or_default(),
        worktree_path: admin.path.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockWorktreePlan {
    pub lock_file: PathBuf,
    pub worktree_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnlockWorktreeError;

pub fn plan_unlock(
    admin: &LinkedWorktreeAdmin,
) -> std::result::Result<UnlockWorktreePlan, UnlockWorktreeError> {
    if admin.locked_reason.is_none() {
        return Err(UnlockWorktreeError);
    }
    Ok(UnlockWorktreePlan {
        lock_file: admin.admin_dir.join("locked"),
        worktree_path: admin.path.clone(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveWorktreePlan {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub admin_dir: PathBuf,
    pub relative_paths: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedWorktreeError {
    pub reason: String,
    pub required_force: usize,
}

/// Validate the lock/force gate before a move resolves or inspects its
/// destination. Git reports a locked source before destination errors.
pub fn plan_move_access(
    admin: &LinkedWorktreeAdmin,
    force: usize,
) -> std::result::Result<(), ProtectedWorktreeError> {
    ensure_lock_override(admin, force)
}

pub fn plan_move(
    admin: &LinkedWorktreeAdmin,
    destination: PathBuf,
    force: usize,
    relative_paths: bool,
) -> std::result::Result<MoveWorktreePlan, ProtectedWorktreeError> {
    plan_move_access(admin, force)?;
    Ok(MoveWorktreePlan {
        source: admin.path.clone(),
        destination,
        admin_dir: admin.admin_dir.clone(),
        relative_paths,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveWorktreePlan {
    pub worktree_path: PathBuf,
    pub admin_dir: PathBuf,
    pub force: usize,
}

pub fn plan_remove(
    admin: &LinkedWorktreeAdmin,
    force: usize,
) -> std::result::Result<RemoveWorktreePlan, ProtectedWorktreeError> {
    ensure_lock_override(admin, force)?;
    Ok(RemoveWorktreePlan {
        worktree_path: admin.path.clone(),
        admin_dir: admin.admin_dir.clone(),
        force,
    })
}

fn ensure_lock_override(
    admin: &LinkedWorktreeAdmin,
    force: usize,
) -> std::result::Result<(), ProtectedWorktreeError> {
    if let Some(reason) = admin.locked_reason.as_ref()
        && force < 2
    {
        return Err(ProtectedWorktreeError {
            reason: reason.clone(),
            required_force: 2,
        });
    }
    Ok(())
}

fn read_linked_worktree_admin(admin_dir: &Path) -> Result<Option<LinkedWorktreeAdmin>> {
    let gitdir_file = admin_dir.join("gitdir");
    if !gitdir_file.is_file() {
        return Ok(None);
    }
    let value = fs::read_to_string(gitdir_file)?;
    let gitdir = resolve_admin_path(admin_dir, value.trim());
    let Some(path) = gitdir.parent() else {
        return Ok(None);
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let locked_reason = read_lock_reason(admin_dir)?;
    let prunable_reason = (locked_reason.is_none() && !gitdir.exists())
        .then(|| "gitdir file points to non-existent location".to_string());
    let admin_name = admin_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Some(LinkedWorktreeAdmin {
        admin_dir: admin_dir.to_path_buf(),
        admin_name,
        path,
        prunable_reason,
        locked_reason,
    }))
}

fn resolve_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

fn resolve_admin_path_forgiving(admin_dir: &Path, value: &str) -> PathBuf {
    let resolved = resolve_admin_path(admin_dir, value);
    fs::canonicalize(&resolved).unwrap_or_else(|_| normalize_lexical_path(&resolved))
}

fn read_lock_reason(admin_dir: &Path) -> Result<Option<String>> {
    let locked = admin_dir.join("locked");
    if !locked.is_file() {
        return Ok(None);
    }
    Ok(Some(
        fs::read_to_string(locked)?
            .trim_end_matches('\n')
            .to_string(),
    ))
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin(locked_reason: Option<&str>) -> LinkedWorktreeAdmin {
        LinkedWorktreeAdmin {
            admin_dir: PathBuf::from("/repo/.git/worktrees/linked"),
            admin_name: "linked".to_string(),
            path: PathBuf::from("/repo/linked"),
            prunable_reason: None,
            locked_reason: locked_reason.map(str::to_string),
        }
    }

    #[test]
    fn all_admin_operations_have_typed_plans_and_outcomes() {
        let admin = admin(None);
        let add = plan_add(PathBuf::from("/repo/new"), 1, true, Some("hold"));
        assert_eq!(add.lock_contents, Some(b"hold\n".to_vec()));

        let snapshot = WorktreeAdminSnapshot {
            linked: vec![admin.clone()],
        };
        assert_eq!(plan_list(&snapshot, true).linked, vec![admin.clone()]);
        assert_eq!(
            plan_add_registration(&snapshot, &admin.path, 0),
            Err(AddRegistrationError {
                locked: false,
                required_force: 1,
            })
        );
        assert_eq!(
            plan_add_registration(&snapshot, &admin.path, 1),
            Ok(AddRegistrationPlan::Replace {
                admin_dir: admin.admin_dir.clone(),
            })
        );
        assert_eq!(plan_prune(true, false, 42).expire, 42);
        assert_eq!(
            plan_repair(vec![(admin.path.clone(), None)], true)
                .targets
                .len(),
            1
        );
        assert_eq!(
            plan_lock(&admin, Some("hold"))
                .expect("unlocked worktree")
                .contents,
            b"hold\n"
        );
        assert_eq!(
            plan_move(&admin, PathBuf::from("/repo/moved"), 0, true)
                .expect("movable worktree")
                .destination,
            PathBuf::from("/repo/moved")
        );
        assert_eq!(
            plan_remove(&admin, 0)
                .expect("removable worktree")
                .worktree_path,
            admin.path
        );

        let locked = LinkedWorktreeAdmin {
            locked_reason: Some("hold".to_string()),
            ..admin
        };
        assert_eq!(
            plan_unlock(&locked).expect("locked worktree").lock_file,
            locked.admin_dir.join("locked")
        );
        assert!(matches!(
            plan_move(&locked, PathBuf::from("/repo/moved"), 1, false),
            Err(ProtectedWorktreeError {
                required_force: 2,
                ..
            })
        ));

        let outcomes = [
            WorktreeAdminOutcome::Added {
                path: PathBuf::new(),
            },
            WorktreeAdminOutcome::Listed { count: 1 },
            WorktreeAdminOutcome::Moved {
                from: PathBuf::new(),
                to: PathBuf::new(),
            },
            WorktreeAdminOutcome::Removed {
                path: PathBuf::new(),
            },
            WorktreeAdminOutcome::Locked {
                path: PathBuf::new(),
            },
            WorktreeAdminOutcome::Unlocked {
                path: PathBuf::new(),
            },
            WorktreeAdminOutcome::Pruned,
            WorktreeAdminOutcome::Repaired,
        ];
        assert_eq!(
            outcomes.map(|outcome| outcome.operation()),
            [
                WorktreeAdminOperation::Add,
                WorktreeAdminOperation::List,
                WorktreeAdminOperation::Move,
                WorktreeAdminOperation::Remove,
                WorktreeAdminOperation::Lock,
                WorktreeAdminOperation::Unlock,
                WorktreeAdminOperation::Prune,
                WorktreeAdminOperation::Repair,
            ]
        );
    }

    #[test]
    fn snapshot_scans_once_and_matches_missing_paths_lexically() {
        let root = tempfile::tempdir().expect("temporary repository");
        let admin_dir = root.path().join("worktrees/linked");
        fs::create_dir_all(&admin_dir).expect("admin directory");
        fs::write(admin_dir.join("gitdir"), "/missing/linked/.git\n").expect("gitdir");
        fs::write(admin_dir.join("locked"), "because\n").expect("lock");

        let snapshot = WorktreeAdminSnapshot::scan(root.path()).expect("scan admins");
        assert_eq!(snapshot.linked().len(), 1);
        let linked = snapshot
            .find_path(Path::new("/missing/other/../linked"))
            .expect("lexical match");
        assert_eq!(linked.locked_reason.as_deref(), Some("because"));
    }

    #[test]
    fn prune_plans_locked_missing_and_duplicate_admins() {
        let root = tempfile::tempdir().expect("temporary repository");
        let locked = root.path().join("worktrees/locked");
        fs::create_dir_all(&locked).expect("locked admin");
        fs::write(locked.join("locked"), b"hold\n").expect("lock");
        assert_eq!(
            plan_prune_admin(&locked, i64::MAX).expect("prune decision"),
            PruneAdminDecision::Skip
        );

        let missing = root.path().join("worktrees/missing");
        fs::create_dir_all(&missing).expect("missing admin");
        fs::write(missing.join("gitdir"), "/does/not/exist/.git\n").expect("gitdir");
        assert_eq!(
            plan_prune_admin(&missing, i64::MAX).expect("prune decision"),
            PruneAdminDecision::Prune("gitdir file points to non-existent location".to_string())
        );

        let path = PathBuf::from("/repo/main");
        assert_eq!(
            plan_duplicate_prunes(vec![
                PruneKeptWorktree {
                    path: path.clone(),
                    admin_name: Some("linked-b".to_string()),
                },
                PruneKeptWorktree {
                    path: path.clone(),
                    admin_name: None,
                },
                PruneKeptWorktree {
                    path,
                    admin_name: Some("linked-a".to_string()),
                },
            ]),
            vec!["linked-a".to_string(), "linked-b".to_string()]
        );
    }
}
