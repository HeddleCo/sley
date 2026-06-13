use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sley::{
    BString, Index, IndexEntry, IndexStage, IndexWriteError, IndexWriteOptions, ObjectFormat,
    ObjectId, Repository,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "sley-index-io-{}-{}",
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

fn index_path(repo: &Repository) -> PathBuf {
    sley::plumbing::sley_worktree::repository_index_path(repo.git_dir())
}

fn index_lock_path(repo: &Repository) -> PathBuf {
    let path = index_path(repo);
    let mut lock_name = path.file_name().expect("index filename").to_os_string();
    lock_name.push(".lock");
    path.with_file_name(lock_name)
}

fn test_entry(path: &str, mode: u32, stage: IndexStage) -> IndexEntry {
    let path_bytes = path.as_bytes();
    let flags = (path_bytes.len().min(0x0fff) as u16) | (stage.as_u16() << 12);
    IndexEntry {
        ctime_seconds: 1,
        ctime_nanoseconds: 2,
        mtime_seconds: 3,
        mtime_nanoseconds: 4,
        dev: 5,
        ino: 6,
        mode,
        uid: 7,
        gid: 8,
        size: 9,
        oid: ObjectId::empty_blob(ObjectFormat::Sha1),
        flags,
        flags_extended: 0,
        path: BString::from(path),
    }
}

fn test_index(entries: Vec<IndexEntry>) -> Index {
    Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    }
}

#[test]
fn repository_write_index_round_trips_entries_and_checksum() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let index = test_index(vec![
        test_entry("normal.txt", 0o100644, IndexStage::Normal),
        test_entry("conflicted.txt", 0o100755, IndexStage::Ours),
    ]);

    repo.write_index(
        &index,
        IndexWriteOptions {
            fsync: false,
            validate_checksum: true,
        },
    )
    .expect("write index");

    let read = repo.read_index().expect("read index");
    assert_eq!(read.version, 2);
    assert_eq!(read.entries.len(), 2);
    assert_eq!(read.entries[0].path, BString::from("normal.txt"));
    assert_eq!(read.entries[0].mode, 0o100644);
    assert_eq!(read.entries[0].stage(), IndexStage::Normal);
    assert_eq!(read.entries[1].path, BString::from("conflicted.txt"));
    assert_eq!(read.entries[1].mode, 0o100755);
    assert_eq!(read.entries[1].stage(), IndexStage::Ours);
    assert!(read.checksum.is_some());
    assert!(!index_lock_path(&repo).exists());
}

#[test]
fn repository_write_index_existing_lock_fails_and_preserves_index() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let initial = test_index(vec![test_entry("kept.txt", 0o100644, IndexStage::Normal)]);
    repo.write_index(&initial, IndexWriteOptions::default())
        .expect("write initial index");
    fs::write(index_lock_path(&repo), b"held\n").expect("create lock");

    let replacement = test_index(vec![test_entry(
        "replacement.txt",
        0o100644,
        IndexStage::Normal,
    )]);
    let err = repo
        .write_index(&replacement, IndexWriteOptions::default())
        .expect_err("held lock must fail");

    assert!(matches!(err, IndexWriteError::ExistingLock));
    let read = repo.read_index().expect("read preserved index");
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].path, BString::from("kept.txt"));
    assert_eq!(
        fs::read(index_lock_path(&repo)).expect("read lock"),
        b"held\n"
    );
}

#[test]
fn repository_write_index_invalid_index_does_not_replace_existing_index() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let initial = test_index(vec![test_entry("kept.txt", 0o100644, IndexStage::Normal)]);
    repo.write_index(&initial, IndexWriteOptions::default())
        .expect("write initial index");
    let mut invalid = test_index(Vec::new());
    invalid.version = 99;

    let err = repo
        .write_index(&invalid, IndexWriteOptions::default())
        .expect_err("invalid index must fail");

    assert!(matches!(err, IndexWriteError::Unsupported(_)));
    let read = repo.read_index().expect("read preserved index");
    assert_eq!(read.entries.len(), 1);
    assert_eq!(read.entries[0].path, BString::from("kept.txt"));
    assert!(!index_lock_path(&repo).exists());
}

#[test]
fn repository_read_index_missing_returns_not_found() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let err = repo.read_index().expect_err("missing index");
    assert!(matches!(err, sley::IndexError::NotFound));
}
