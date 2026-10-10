#![cfg(unix)]

use sley_config::GitConfig;
use sley_core::diagnostics::{DiagnosticSink, DiagnosticStream, Diagnostics};
use sley_core::{BString, ObjectFormat};
use sley_index::Index;
use sley_object::{EncodedObject, ObjectType, Tree, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectWriter};
use sley_sequencer::apply::merge_index_entry;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<u8>>>);

impl DiagnosticSink for Warnings {
    fn write(&self, stream: DiagnosticStream, bytes: &[u8]) -> std::io::Result<()> {
        assert_eq!(stream, DiagnosticStream::Stderr);
        self.0
            .lock()
            .expect("warnings lock")
            .extend_from_slice(bytes);
        Ok(())
    }
}

fn check_rmdir_failure(hard: bool, prune: bool) {
    let root = tempfile::tempdir().expect("worktree");
    let git_dir = root.path().join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("object directory");
    fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("HEAD");
    fs::write(git_dir.join("config"), b"[core]\n bare = false\n").expect("config");
    let format = ObjectFormat::Sha1;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let old = db
        .write_object(EncodedObject::new(ObjectType::Blob, b"old".to_vec()))
        .expect("old blob");
    let new = db
        .write_object(EncodedObject::new(ObjectType::Blob, b"new".to_vec()))
        .expect("new blob");
    let tree = db
        .write_object(EncodedObject::new(
            ObjectType::Tree,
            Tree {
                entries: vec![TreeEntry {
                    mode: 0o100644,
                    name: BString::from(b"z-file".as_slice()),
                    oid: new,
                }],
            }
            .write(),
        ))
        .expect("target tree");
    let target = db
            .write_object(EncodedObject::new(
                ObjectType::Commit,
                format!("tree {tree}\nauthor A <a@example.com> 0 +0000\ncommitter A <a@example.com> 0 +0000\n\ntarget\n").into_bytes(),
            ))
            .expect("target commit");
    fs::create_dir_all(root.path().join("locked/module")).expect("directory leaf");
    let (path, mode) = if prune {
        fs::write(root.path().join("locked/module/file"), b"old").expect("nested file");
        (b"locked/module/file".as_slice(), 0o100644)
    } else {
        (b"locked/module".as_slice(), 0o160000)
    };
    fs::write(root.path().join("y-later"), b"old").expect("later deletion");
    let index = Index {
        version: 2,
        entries: vec![
            merge_index_entry(path, mode, if prune { old } else { target }, 0),
            merge_index_entry(b"y-later", 0o100644, old, 0),
        ],
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        git_dir.join("index"),
        index.write(format).expect("encode index"),
    )
    .expect("old index");
    let locked = root.path().join("locked");
    fs::create_dir(locked.join("probe")).expect("empty rmdir probe");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("deny rmdir");
    // Verify that the permission change actually injects a directory-removal failure.
    let probe = fs::remove_dir(locked.join("probe"));
    assert_eq!(
        probe.expect_err("rmdir must fail").kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let warnings = Warnings::default();
    let result = Diagnostics::new(warnings.clone()).scope(|| {
        if hard {
            sley_worktree::reset_index_and_worktree_to_commit(
                None,
                root.path(),
                &git_dir,
                format,
                &target,
            )
            .map(|_| ())
        } else {
            sley_sequencer::pick::reset_merge_in(
                None,
                &git_dir,
                root.path(),
                format,
                Some(&target),
                &GitConfig::default(),
                None,
            )
        }
    });
    // Restore permissions before assertions so a failing regression cleans up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore permissions");
    result.expect("directory removal failure must warn and continue");
    let warning =
        String::from_utf8(warnings.0.lock().expect("warnings lock").clone()).expect("warning text");
    assert!(warning.contains("warning: unable to rmdir"), "{warning:?}");
    assert!(warning.contains("locked/module"), "{warning:?}");
    let index = Index::parse(&fs::read(git_dir.join("index")).expect("new index"), format)
        .expect("parse index");
    assert_eq!(index.entries.len(), 1);
    assert_eq!(index.entries[0].path.as_bytes(), b"z-file");
    assert_eq!(index.entries[0].oid, new);
    assert_eq!(
        fs::read(root.path().join("z-file")).expect("materialized target"),
        b"new"
    );
    assert!(!root.path().join("y-later").exists());
    assert_eq!(
        fs::read_dir(root.path().join("locked/module"))
            .expect("retained empty directory")
            .count(),
        0
    );
}

#[test]
fn merge_reset_warns_and_continues_after_rmdir_failure() {
    check_rmdir_failure(false, false);
}

#[test]
fn hard_reset_warns_and_continues_after_rmdir_failure() {
    check_rmdir_failure(true, false);
}

#[test]
fn merge_reset_warns_and_continues_after_parent_rmdir_failure() {
    check_rmdir_failure(false, true);
}

#[test]
fn hard_reset_warns_and_continues_after_parent_rmdir_failure() {
    check_rmdir_failure(true, true);
}

#[test]
fn merge_removal_still_reports_regular_file_unlink_failure() {
    let root = tempfile::tempdir().expect("worktree");
    let locked = root.path().join("locked");
    fs::create_dir(&locked).expect("parent directory");
    fs::write(locked.join("file"), b"keep").expect("regular file");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("deny unlink");
    let warnings = Warnings::default();
    let result = Diagnostics::new(warnings.clone()).scope(|| {
        sley_sequencer::apply::merge_remove_worktree_file(None, root.path(), b"locked/file")
    });
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("restore permissions");
    assert!(result.is_err(), "regular-file errors still propagate");
    assert!(warnings.0.lock().expect("warnings lock").is_empty());
    assert_eq!(
        fs::read(locked.join("file")).expect("retained file"),
        b"keep"
    );
}
