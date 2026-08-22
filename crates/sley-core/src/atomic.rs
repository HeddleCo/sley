//! Atomic file publication primitives: lock-file guarded writes published by
//! rename.
//!
//! Consolidates the hand-rolled `create_new(temp)` + optional barrier +
//! `rename(target)` dances scattered across writers. The [`LockFile`] guard
//! owns the exclusive-create temp/lock file and removes it on drop unless the
//! write is explicitly persisted (or deliberately kept, for `mkstemp`-style
//! creators whose temporary name is the deliverable).
//!
//! Naming conventions stay at call sites: [`LockFile::acquire`] uses git's
//! sibling `<name>.lock` convention, while [`LockFile::create`] accepts an
//! exact path for load-bearing names such as odb `tmp_obj_<pid>_<n>` temps.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{fsync, GitError, Result};

/// Lock path for `path`: the file name suffixed `.lock` in the same directory
/// (git's lockfile convention, e.g. `packed-refs` -> `packed-refs.lock`).
pub fn lock_path_for(path: &Path) -> Result<PathBuf> {
    let Some(file_name) = path.file_name() else {
        return Err(GitError::InvalidPath(format!(
            "path has no filename: {}",
            path.display()
        )));
    };
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

/// An exclusively created lock/temp file with atomic publication semantics.
///
/// Dropping an unpersisted guard closes the handle and removes the file, so
/// failure paths never leave stale locks behind. Persisting renames the file
/// over its target, which is atomic within a filesystem.
#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
    /// Publication target remembered by [`Self::acquire`]; absent for
    /// `create`-built guards, which publish through [`Self::persist_into`].
    target: Option<PathBuf>,
    file: Option<fs::File>,
    armed: bool,
}

impl LockFile {
    /// Exclusively create the lock at exactly `path`; fails when it already
    /// exists (`AlreadyExists` kind, inspectable via
    /// [`GitError::io_kind`](crate::GitError::io_kind)).
    pub fn create(path: PathBuf) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            target: None,
            file: Some(file),
            armed: true,
        })
    }

    /// Exclusively create the sibling `<target>.lock` lock for `target`.
    pub fn acquire(target: &Path) -> Result<Self> {
        let mut lock = Self::create(lock_path_for(target)?)?;
        lock.target = Some(target.to_path_buf());
        Ok(lock)
    }

    /// The lock file's own path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The publication target this lock was acquired for, when known.
    pub fn target(&self) -> Option<&Path> {
        self.target.as_deref()
    }

    /// Mutable handle for streaming or filter-style writers.
    pub fn file_mut(&mut self) -> Option<&mut fs::File> {
        self.file.as_mut()
    }

    /// Write the full payload through the lock handle.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(GitError::Io("lock file is already closed".into()));
        };
        file.write_all(bytes)?;
        Ok(())
    }

    /// Apply `policy`'s barrier for `component` to the lock handle. No-op
    /// when the policy excludes the component or the test switch disables
    /// syncing.
    pub fn sync(&mut self, policy: &fsync::Policy, component: fsync::FsyncComponents) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Err(GitError::Io("lock file is already closed".into()));
        };
        policy.apply(file, component)?;
        Ok(())
    }

    /// Publish onto the target remembered by [`Self::acquire`]: rename the
    /// lock over it, removing the lock file if the rename fails.
    pub fn persist(self) -> Result<()> {
        let Some(target) = self.target.clone() else {
            return Err(GitError::Io(format!(
                "lock file {} has no publication target",
                self.path.display()
            )));
        };
        self.persist_into(&target)
    }

    /// Publish onto `target`: rename the lock over it. On rename failure the
    /// lock file is removed and the original error surfaces as
    /// [`GitError::Io`], matching the pre-existing dance's cleanup order.
    pub fn persist_into(mut self, target: &Path) -> Result<()> {
        self.armed = false;
        let _ = self.file.take();
        match fs::rename(&self.path, target) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_file(&self.path);
                Err(GitError::Io(err.to_string()))
            }
        }
    }

    /// Publish tolerating a concurrent writer that landed first: when the
    /// rename fails but `target` now exists, treat the write as won-by-other
    /// and succeed after removing our temp file. This matches the loose
    /// object store's race handling, where two processes may materialize the
    /// same content-addressed object simultaneously.
    pub fn persist_racy(mut self, target: &Path) -> Result<()> {
        self.armed = false;
        let _ = self.file.take();
        match fs::rename(&self.path, target) {
            Ok(()) => Ok(()),
            Err(_) if target.exists() => {
                let _ = fs::remove_file(&self.path);
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(&self.path);
                Err(GitError::Io(err.to_string()))
            }
        }
    }

    /// Disarm drop-cleanup and hand back the (path, handle) pair for
    /// `mkstemp`-style creators whose temporary file must survive the guard.
    pub fn keep(mut self) -> (PathBuf, Option<fs::File>) {
        self.armed = false;
        (std::mem::take(&mut self.path), self.file.take())
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            let _ = self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Atomically replace `path` with `contents`: sibling `.lock` temp, write,
/// rename. Fails when the lock already exists; callers create parent
/// directories themselves when needed.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_with(path, |file| {
        file.write_all(contents)?;
        Ok(())
    })
}

/// Atomically replace `path` using a writer callback, so streamed encoders
/// avoid buffering the whole payload twice.
pub fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<()> {
    let mut lock = LockFile::acquire(path)?;
    match lock.file_mut() {
        Some(file) => write(file)?,
        None => return Err(GitError::Io("lock file is already closed".into())),
    }
    lock.persist()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sley-core-atomic-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn lock_path_for_appends_lock_suffix() {
        assert_eq!(
            lock_path_for(Path::new("refs/heads/main")).expect("lock path"),
            PathBuf::from("refs/heads/main.lock")
        );
        assert!(lock_path_for(Path::new("")).is_err());
    }

    #[test]
    fn acquire_write_persist_replaces_target_atomically() {
        let dir = scratch_dir("persist");
        let target = dir.join("packed-refs");
        fs::write(&target, b"old").expect("seed target");

        let mut lock = LockFile::acquire(&target).expect("acquire");
        assert_eq!(lock.path(), target.with_file_name("packed-refs.lock"));
        lock.write_all(b"new").expect("write");
        lock.persist().expect("persist");

        assert_eq!(fs::read(&target).expect("read target"), b"new");
        assert!(!target.with_file_name("packed-refs.lock").exists());

        // A second acquisition must observe the published content.
        let mut again = LockFile::acquire(&target).expect("re-acquire");
        again.write_all(b"newer").expect("write");
        again.persist().expect("persist");
        assert_eq!(fs::read(&target).expect("read target"), b"newer");

        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn dropped_guard_removes_lock_and_atomic_write_fails_when_locked() {
        let dir = scratch_dir("drop");
        let target = dir.join("HEAD");
        let guard = LockFile::acquire(&target).expect("acquire");
        let lock_path = guard.path().to_path_buf();
        drop(guard);
        assert!(!lock_path.exists(), "dropped guard must remove its lock");

        // Held locks reject concurrent writers instead of clobbering.
        let held = LockFile::acquire(&target).expect("hold");
        let err = atomic_write(&target, b"x").expect_err("second writer must fail");
        assert_eq!(err.io_kind(), Some(std::io::ErrorKind::AlreadyExists));
        drop(held);

        atomic_write(&target, b"payload").expect("atomic write after release");
        assert_eq!(fs::read(&target).expect("read"), b"payload");
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn create_keeps_exact_temp_names_for_mkstemp_style_callers() {
        let dir = scratch_dir("keep");
        let temp = dir.join("tmp_obj_42_7");
        let lock = LockFile::create(temp.clone()).expect("create");
        assert!(temp.exists());
        let (kept_path, _) = lock.keep();
        assert_eq!(kept_path, temp);
        assert!(temp.exists(), "keep() must not delete the file");

        // Double-create is rejected with AlreadyExists.
        let err = LockFile::create(temp.clone()).expect_err("exclusive create");
        assert_eq!(err.io_kind(), Some(std::io::ErrorKind::AlreadyExists));
        fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn persist_racy_succeeds_when_another_writer_landed_first() {
        let dir = scratch_dir("racy");
        let target = dir.join("objects/ab/cdef");
        fs::create_dir_all(target.parent().expect("parent")).expect("fanout");
        fs::write(&target, b"theirs").expect("concurrent winner");

        let temp = dir.join("tmp_obj_racy");
        let mut lock = LockFile::create(temp).expect("create temp");
        lock.write_all(b"ours").expect("write");
        // POSIX rename replaces the winner's file; Windows refuses to
        // replace and takes the race arm — both are successes.
        lock.persist_racy(&target)
            .expect("racy publish must tolerate the concurrent winner");
        let contents = fs::read(&target).expect("read");
        assert!(contents == b"theirs" || contents == b"ours");
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
