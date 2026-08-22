//! Locked index read/write helpers for embedders.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sley_index::{Index, IndexEntry};

use crate::{GitError, IndexStatProbe, IndexStatProbeCache, Repository};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexWriteOptions {
    pub fsync: bool,
    pub validate_checksum: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWriteResult {
    pub path: PathBuf,
    pub len: u64,
    pub mtime: Option<(u64, u64)>,
}

impl IndexWriteResult {
    pub fn stat_probe_for_entry(&self, entry: IndexEntry) -> IndexStatProbe {
        IndexStatProbe::from_index_entry(entry, self.mtime)
    }
}

#[derive(Debug)]
pub enum IndexError {
    NotFound,
    Io(std::io::Error),
    InvalidIndex(String),
    ChecksumFailure(String),
    Unsupported(String),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("index not found"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::InvalidIndex(message) => write!(f, "invalid index: {message}"),
            Self::ChecksumFailure(message) => write!(f, "index checksum failure: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported index: {message}"),
        }
    }
}

impl std::error::Error for IndexError {}

#[derive(Debug)]
pub enum IndexWriteError {
    ExistingLock,
    InvalidIndex(String),
    ChecksumFailure(String),
    Io(std::io::Error),
    Unsupported(String),
}

impl fmt::Display for IndexWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExistingLock => f.write_str("index lock already exists"),
            Self::InvalidIndex(message) => write!(f, "invalid index: {message}"),
            Self::ChecksumFailure(message) => write!(f, "index checksum failure: {message}"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Unsupported(message) => write!(f, "unsupported index: {message}"),
        }
    }
}

impl std::error::Error for IndexWriteError {}

impl Repository {
    /// Read this repository's index (`.git/index` or `$GIT_INDEX_FILE`).
    pub fn read_index(&self) -> std::result::Result<Index, IndexError> {
        let path = sley_worktree::repository_index_path(&self.git_dir);
        match fs::read(path) {
            Ok(bytes) => Index::parse(&bytes, self.format).map_err(IndexError::from_git_error),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(IndexError::NotFound),
            Err(err) => Err(IndexError::Io(err)),
        }
    }

    /// Read this repository's index once and return reusable stat probes.
    ///
    /// This is the bulk form of [`IndexStatProbe::from_repository_index`].
    /// Embedders that verify many worktree paths should prefer this method so
    /// `.git/index` is parsed once instead of once per path.
    pub fn index_stat_probes(&self) -> std::result::Result<IndexStatProbeCache, IndexError> {
        IndexStatProbeCache::from_repository_index(&self.git_dir, self.format)
            .map_err(IndexError::from_git_error)
    }

    /// Write this repository's index through `index.lock` and an atomic rename.
    pub fn write_index(
        &self,
        index: &Index,
        options: IndexWriteOptions,
    ) -> std::result::Result<(), IndexWriteError> {
        self.write_index_with_result(index, options).map(|_| ())
    }

    /// Write this repository's index through `index.lock` and an atomic rename,
    /// returning the post-write index metadata needed for racy-git probes.
    pub fn write_index_with_result(
        &self,
        index: &Index,
        options: IndexWriteOptions,
    ) -> std::result::Result<IndexWriteResult, IndexWriteError> {
        let path = sley_worktree::repository_index_path(&self.git_dir);
        write_index_locked(&path, index, self.format, options)
    }
}

fn write_index_locked(
    path: &Path,
    index: &Index,
    format: crate::ObjectFormat,
    options: IndexWriteOptions,
) -> std::result::Result<IndexWriteResult, IndexWriteError> {
    let parent = path.parent().ok_or_else(|| {
        IndexWriteError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "index path has no parent",
        ))
    })?;
    fs::create_dir_all(parent).map_err(IndexWriteError::Io)?;

    let lock_path = index_lock_path(path)?;
    let mut lock = IndexLock::acquire(lock_path)?;
    let bytes = index
        .write(format)
        .map_err(IndexWriteError::from_git_error)?;
    if options.validate_checksum {
        Index::parse(&bytes, format).map_err(IndexWriteError::checksum_from_git_error)?;
    }
    lock.write_all(&bytes, options.fsync)?;
    let lock_path = lock.close();
    if let Err(err) = fs::rename(&lock_path, path) {
        let _ = fs::remove_file(&lock_path);
        return Err(IndexWriteError::Io(err));
    }
    let metadata = fs::metadata(path).map_err(IndexWriteError::Io)?;
    Ok(IndexWriteResult {
        path: path.to_path_buf(),
        len: metadata.len(),
        mtime: metadata_mtime(&metadata),
    })
}

fn metadata_mtime(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs(), u64::from(duration.subsec_nanos())))
}

fn index_lock_path(path: &Path) -> std::result::Result<PathBuf, IndexWriteError> {
    let file_name = path.file_name().ok_or_else(|| {
        IndexWriteError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "index path has no filename",
        ))
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

struct IndexLock {
    path: PathBuf,
    file: Option<fs::File>,
    active: bool,
}

impl IndexLock {
    fn acquire(path: PathBuf) -> std::result::Result<Self, IndexWriteError> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => Ok(Self {
                path,
                file: Some(file),
                active: true,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(IndexWriteError::ExistingLock)
            }
            Err(err) => Err(IndexWriteError::Io(err)),
        }
    }

    fn write_all(&mut self, bytes: &[u8], fsync: bool) -> std::result::Result<(), IndexWriteError> {
        let Some(file) = self.file.as_mut() else {
            return Err(IndexWriteError::Io(std::io::Error::other(
                "index lock is already closed",
            )));
        };
        file.write_all(bytes).map_err(IndexWriteError::Io)?;
        if fsync {
            file.sync_all().map_err(IndexWriteError::Io)?;
        }
        Ok(())
    }

    fn close(mut self) -> PathBuf {
        self.active = false;
        let _ = self.file.take();
        self.path.clone()
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if self.active {
            let _ = self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl IndexError {
    fn from_git_error(err: GitError) -> Self {
        match err {
            GitError::InvalidFormat(message) if message.contains("checksum") => {
                Self::ChecksumFailure(message)
            }
            GitError::InvalidFormat(message)
            | GitError::InvalidObjectId(message)
            | GitError::InvalidObject(message)
            | GitError::InvalidPath(message) => Self::InvalidIndex(message),
            GitError::Unsupported(message) => Self::Unsupported(message),
            GitError::Io(message) => Self::Io(std::io::Error::other(message)),
            GitError::IoKind { kind, message } => Self::Io(std::io::Error::new(kind, message)),
            other => Self::InvalidIndex(other.to_string()),
        }
    }
}

impl IndexWriteError {
    fn from_git_error(err: GitError) -> Self {
        match err {
            GitError::InvalidFormat(message) if message.contains("checksum") => {
                Self::ChecksumFailure(message)
            }
            GitError::InvalidFormat(message)
            | GitError::InvalidObjectId(message)
            | GitError::InvalidObject(message)
            | GitError::InvalidPath(message) => Self::InvalidIndex(message),
            GitError::Unsupported(message) => Self::Unsupported(message),
            GitError::Io(message) => Self::Io(std::io::Error::other(message)),
            GitError::IoKind { kind, message } => Self::Io(std::io::Error::new(kind, message)),
            other => Self::InvalidIndex(other.to_string()),
        }
    }

    fn checksum_from_git_error(err: GitError) -> Self {
        match Self::from_git_error(err) {
            Self::InvalidIndex(message) | Self::Unsupported(message) => {
                Self::ChecksumFailure(message)
            }
            other => other,
        }
    }
}
