//! The gitlink worktree-apply primitive — one decision table for the directory
//! management git's `entry.c` performs when a tree-switch (`unpack-trees`-driven
//! checkout / reset / read-tree, and the merge/revert/cherry-pick worktree
//! apply) materializes or drops a *gitlink* (submodule, mode `0o160000`) path.
//!
//! ## Why this is its own primitive
//!
//! A gitlink's oid names a commit in the *submodule's* repository, not a blob
//! in the superproject's object store, so the ordinary "write the blob / unlink
//! the file" worktree apply is wrong for it twice over:
//!
//! - **Appearing** (a gitlink enters the target tree): git creates an *empty
//!   directory* at the path (`entry.c::write_entry` `case S_IFGITLINK: mkdir`),
//!   it does NOT write the commit oid as file bytes. A non-recursing switch
//!   never checks the submodule out — that is `git submodule update`'s job —
//!   so the placeholder is deliberately empty.
//! - **Disappearing** (a gitlink leaves the target tree): git LEAVES the
//!   submodule's worktree directory and its contents in place
//!   (`entry.c::remove_or_warn` → `rmdir_or_warn`, which only removes the dir
//!   when it is empty and merely warns otherwise). It never recurses into a
//!   populated submodule to delete work, even on a forced switch — that would
//!   silently destroy unpushed submodule history.
//!
//! Every tree-applying command needs the *same* rule, so sley keeps it here as
//! one pure decision ([`gitlink_apply`]) plus the I/O wrappers
//! ([`apply_appearing_gitlink`] / [`apply_disappearing_path`]) that perform it.
//! That makes the whole non-recursing submodule class
//! (`t/lib-submodule-update.sh::test_submodule_switch_common`) pass identically
//! across read-tree / checkout / reset / merge from one code path, which is
//! exactly how git derives all of `t1013` / `t2013` / `t7112` / `t6438` from a
//! single `entry.c`.
//!
//! ## Mapping to git
//!
//! | git symbol | here |
//! |---|---|
//! | `write_entry` `case S_IFGITLINK: mkdir(path)` | [`Directive::MakeEmptyDir`] |
//! | `write_entry` D/F: existing dir + `S_ISGITLINK` → "leave it alone" | [`Directive::MakeEmptyDir`] (skip-if-dir) |
//! | `write_entry` `!check_path` + `!populated` + non-dir → `unlink` | the unlink inside [`apply_appearing_gitlink`] |
//! | `remove_or_warn(S_ISGITLINK)` → `rmdir_or_warn` | [`Directive::LeaveDirInPlace`] / [`apply_disappearing_path`] |
//! | `unlink_entry` (gitlink: never recurse) | [`apply_disappearing_path`] |

use std::fs;
use std::io;
use std::path::Path;

use sley_core::{GitError, Result};

/// git's `S_ISGITLINK(mode)`: the entry is a submodule (gitlink) when the file
/// type bits of its raw git mode are `0o160000`.
pub const GITLINK_MODE: u32 = 0o160000;

/// True iff `mode` is a gitlink (submodule) entry.
pub fn is_gitlink(mode: u32) -> bool {
    (mode & 0o170000) == GITLINK_MODE
}

/// The current on-disk state at a worktree path, as the apply decision needs to
/// see it (git's `check_path` / `lstat` result reduced to the cases that change
/// the directive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDisk {
    /// Nothing exists at the path.
    Absent,
    /// An empty directory exists (a leftover placeholder, or one the user
    /// pre-created — git tolerates it for an appearing submodule).
    EmptyDir,
    /// A non-empty directory exists (a populated submodule, or any dir with
    /// contents).
    NonEmptyDir,
    /// A regular file (or symlink) exists at the path.
    File,
}

/// What the caller must do at a gitlink path, the decision git's `entry.c`
/// makes. Pure: the caller performs the I/O (see [`apply_appearing_gitlink`] /
/// [`apply_disappearing_path`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    /// Create an empty directory at the path (an appearing submodule). If a
    /// file is in the way it must first be removed (only when `force` /
    /// untracked-clean authorizes it — the caller resolves that and passes the
    /// effective state).
    MakeEmptyDir,
    /// The path already holds an empty directory that matches the target — do
    /// nothing (git's "existing empty dir is fine" + "if gitlink dir, leave it
    /// alone").
    KeepEmptyDir,
    /// Leave the directory and its contents in place (a disappearing or merely
    /// modified submodule — git never auto-removes a populated submodule on a
    /// non-recursing switch).
    LeaveDirInPlace,
    /// Remove the empty placeholder directory (a disappearing gitlink whose
    /// worktree dir is empty — git's `rmdir_or_warn` succeeds on an empty dir).
    RemoveEmptyDir,
    /// A directory/file conflict that must abort the operation: a populated
    /// submodule cannot be replaced by a tracked file/dir on a non-recursing,
    /// non-forced switch (git refuses, to avoid losing submodule work); or a
    /// tracked file blocks an appearing submodule.
    ConflictRefuse,
}

/// Decide the worktree action for an *appearing* gitlink (a gitlink present in
/// the target tree, mode `0o160000`), given what is on disk now and whether the
/// switch is allowed to discard an in-the-way untracked/tracked file (`force`).
///
/// Ports `entry.c::write_entry`'s `S_IFGITLINK` arm plus the D/F branches of
/// `checkout_entry`:
/// - empty existing dir → keep it (git's `if (S_ISGITLINK) return 0;` for an
///   existing dir, and "doesn't care if it already exists");
/// - non-empty dir → keep it (already-populated submodule, leave alone);
/// - file in the way → remove + mkdir when `force` (forced switch / untracked
///   adoption), else refuse;
/// - absent → mkdir.
pub fn gitlink_apply_appearing(on_disk: OnDisk, force: bool) -> Directive {
    match on_disk {
        OnDisk::Absent => Directive::MakeEmptyDir,
        OnDisk::EmptyDir => Directive::KeepEmptyDir,
        // A populated directory at the gitlink path: git leaves it alone (it is
        // the submodule's working tree; `git submodule update` will sync it).
        OnDisk::NonEmptyDir => Directive::LeaveDirInPlace,
        OnDisk::File => {
            if force {
                Directive::MakeEmptyDir
            } else {
                Directive::ConflictRefuse
            }
        }
    }
}

/// Decide the worktree action for a *disappearing* gitlink (a path that was a
/// gitlink in the index/old tree and is being removed from the worktree).
///
/// Ports `entry.c::remove_or_warn(S_ISGITLINK) -> rmdir_or_warn` and
/// `unlink_entry`'s gitlink handling: an empty submodule dir is rmdir'd, a
/// populated one is left in place (and a warning is emitted upstream — sley
/// stays silent, matching the test contract which only checks the dir survives).
/// Git NEVER recurses to delete a populated submodule on a non-recursing
/// switch, even forced.
pub fn gitlink_apply_disappearing(on_disk: OnDisk) -> Directive {
    match on_disk {
        OnDisk::Absent => Directive::KeepEmptyDir, // nothing to do
        OnDisk::EmptyDir => Directive::RemoveEmptyDir,
        OnDisk::NonEmptyDir => Directive::LeaveDirInPlace,
        // A file where a gitlink was tracked: the ordinary unlink path handles
        // it; the caller only routes here when the path is a directory, but be
        // explicit so the table is total.
        OnDisk::File => Directive::LeaveDirInPlace,
    }
}

/// Probe the on-disk state at `full` (an absolute worktree path).
pub fn probe_on_disk(full: &Path) -> Result<OnDisk> {
    match fs::symlink_metadata(full) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(OnDisk::Absent),
        Err(err) => Err(err.into()),
        Ok(meta) => {
            if meta.is_dir() {
                let mut entries = fs::read_dir(full)?;
                if entries.next().is_some() {
                    Ok(OnDisk::NonEmptyDir)
                } else {
                    Ok(OnDisk::EmptyDir)
                }
            } else {
                Ok(OnDisk::File)
            }
        }
    }
}

/// Materialize an appearing gitlink at `full`: create the empty placeholder
/// directory git leaves for a non-recursing switch, tolerating a pre-existing
/// empty dir, leaving a populated submodule alone, and (when `force`) replacing
/// a file in the way. Returns `Ok(false)` and writes nothing when a D/F
/// conflict refuses the switch.
///
/// This is the single I/O entry point every worktree-apply consumer calls for a
/// `mode == 0o160000` write, so the empty-dir contract is identical across
/// checkout / reset / read-tree / merge.
pub fn apply_appearing_gitlink(full: &Path, force: bool) -> Result<bool> {
    let on_disk = probe_on_disk(full)?;
    match gitlink_apply_appearing(on_disk, force) {
        Directive::MakeEmptyDir => {
            if on_disk == OnDisk::File {
                // Forced: discard the in-the-way file before the mkdir.
                fs::remove_file(full)?;
            }
            // Create the empty placeholder (and any missing leading dirs).
            fs::create_dir_all(full)?;
            Ok(true)
        }
        Directive::KeepEmptyDir | Directive::LeaveDirInPlace => {
            // An empty existing dir or a populated submodule: leave as-is. Make
            // sure leading directories exist for the absent->empty edge.
            if on_disk == OnDisk::Absent {
                fs::create_dir_all(full)?;
            }
            Ok(true)
        }
        Directive::ConflictRefuse => Ok(false),
        Directive::RemoveEmptyDir => {
            // Not reachable for an appearing gitlink, but keep total.
            Ok(true)
        }
    }
}

/// Remove a *disappearing* gitlink's worktree entry at `full` following git's
/// `rmdir_or_warn` semantics: rmdir an empty placeholder, leave a populated
/// submodule (and its `.git`) in place. Never recurses. Returns `Ok(())` in all
/// non-error cases (a populated dir is a successful no-op, matching git which
/// only warns).
///
/// The caller routes here only when the on-disk path is a directory (a gitlink);
/// a plain file at a former gitlink path is handled by the ordinary unlink.
pub fn apply_disappearing_gitlink(full: &Path) -> Result<()> {
    let on_disk = probe_on_disk(full)?;
    match gitlink_apply_disappearing(on_disk) {
        Directive::RemoveEmptyDir => match fs::remove_dir(full) {
            Ok(()) => Ok(()),
            // Raced into non-empty, or vanished: both are fine.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) =>
            {
                Ok(())
            }
            Err(err) => Err(err.into()),
        },
        // Populated submodule (or absent / file): leave in place.
        Directive::LeaveDirInPlace | Directive::KeepEmptyDir => Ok(()),
        Directive::MakeEmptyDir | Directive::ConflictRefuse => {
            // Unreachable for the disappearing table; treat as no-op.
            Ok(())
        }
    }
}

/// Convenience: format a non-UTF-8 worktree path error consistently with the
/// CLI consumers (kept here so the wrappers above can be called with a byte
/// path when a consumer has one).
pub fn worktree_join(worktree_root: &Path, path: &[u8]) -> Result<std::path::PathBuf> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    Ok(worktree_root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn is_gitlink_matches_mode() {
        assert!(is_gitlink(0o160000));
        assert!(!is_gitlink(0o100644));
        assert!(!is_gitlink(0o100755));
        assert!(!is_gitlink(0o120000)); // symlink
        assert!(!is_gitlink(0o040000)); // tree
    }

    // ---- appearing decision table -------------------------------------

    #[test]
    fn appearing_absent_makes_empty_dir() {
        assert_eq!(
            gitlink_apply_appearing(OnDisk::Absent, false),
            Directive::MakeEmptyDir
        );
    }

    #[test]
    fn appearing_tolerates_existing_empty_dir() {
        assert_eq!(
            gitlink_apply_appearing(OnDisk::EmptyDir, false),
            Directive::KeepEmptyDir
        );
    }

    #[test]
    fn appearing_leaves_populated_dir_alone() {
        assert_eq!(
            gitlink_apply_appearing(OnDisk::NonEmptyDir, false),
            Directive::LeaveDirInPlace
        );
    }

    #[test]
    fn appearing_file_refuses_unforced_but_replaces_forced() {
        assert_eq!(
            gitlink_apply_appearing(OnDisk::File, false),
            Directive::ConflictRefuse
        );
        assert_eq!(
            gitlink_apply_appearing(OnDisk::File, true),
            Directive::MakeEmptyDir
        );
    }

    // ---- disappearing decision table ----------------------------------

    #[test]
    fn disappearing_empty_dir_is_removed() {
        assert_eq!(
            gitlink_apply_disappearing(OnDisk::EmptyDir),
            Directive::RemoveEmptyDir
        );
    }

    #[test]
    fn disappearing_populated_dir_is_left_in_place() {
        assert_eq!(
            gitlink_apply_disappearing(OnDisk::NonEmptyDir),
            Directive::LeaveDirInPlace
        );
    }

    // ---- I/O wrappers --------------------------------------------------

    #[test]
    fn apply_appearing_creates_empty_dir() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        assert!(apply_appearing_gitlink(&p, false).expect("apply"));
        assert!(p.is_dir());
        assert_eq!(fs::read_dir(&p).expect("read_dir").count(), 0, "empty placeholder");
    }

    #[test]
    fn apply_appearing_tolerates_preexisting_empty_dir() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::create_dir(&p).expect("mkdir");
        assert!(apply_appearing_gitlink(&p, false).expect("apply"));
        assert!(p.is_dir());
    }

    #[test]
    fn apply_appearing_leaves_populated_submodule() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::create_dir(&p).expect("mkdir");
        fs::write(p.join("file"), b"work").expect("write file");
        assert!(apply_appearing_gitlink(&p, false).expect("apply"));
        assert!(p.join("file").exists(), "populated submodule untouched");
    }

    #[test]
    fn apply_appearing_file_refuses_unforced() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::write(&p, b"content").expect("write content");
        assert!(!apply_appearing_gitlink(&p, false).expect("apply"));
        assert!(p.is_file(), "file untouched on refusal");
    }

    #[test]
    fn apply_appearing_file_replaced_when_forced() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::write(&p, b"content").expect("write content");
        assert!(apply_appearing_gitlink(&p, true).expect("apply"));
        assert!(p.is_dir(), "file replaced by empty dir when forced");
        assert_eq!(fs::read_dir(&p).expect("read_dir").count(), 0);
    }

    #[test]
    fn apply_disappearing_removes_empty_dir() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::create_dir(&p).expect("mkdir");
        apply_disappearing_gitlink(&p).expect("apply");
        assert!(!p.exists(), "empty placeholder removed");
    }

    #[test]
    fn apply_disappearing_leaves_populated_dir() {
        let tmp = tempdir();
        let p = tmp.join("sub1");
        fs::create_dir(&p).expect("mkdir");
        fs::write(p.join("file"), b"work").expect("write file");
        apply_disappearing_gitlink(&p).expect("apply");
        assert!(p.exists(), "populated submodule left in place");
        assert!(p.join("file").exists(), "submodule contents preserved");
    }

    #[test]
    fn apply_disappearing_absent_is_noop() {
        let tmp = tempdir();
        let p = tmp.join("missing");
        apply_disappearing_gitlink(&p).expect("apply");
        assert!(!p.exists());
    }

    // Minimal unique tempdir without pulling in a dev-dependency.
    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "sley-submodule-apply-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let dir = base.join(unique);
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }
}
