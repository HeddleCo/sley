//! Deleting a ref whose on-disk name fails `check_refname_format`.
//!
//! Git gates deletion on `refname_is_safe` rather than on
//! `check_refname_format`, so a broken loose ref such as
//! `refs/heads/broken...ref` can still be removed with `git update-ref -d`.
//! sley must allow the same, through both `delete_ref` and `apply_ref_batch`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sley::{DeleteRef, DeleteRefName, FullName, RefBatchChange, Repository};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sley-delete-broken-ref-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository with one commit and loose refs planted under names that
/// `git update-ref` itself refuses to create. Returns (worktree, oid hex).
fn repo_with_broken_refs(label: &str, names: &[&str]) -> (TempDir, String) {
    let dir = TempDir::new(label);
    git(&dir.path, &["init", "-q", "-b", "main"]);
    git(&dir.path, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let oid = git(&dir.path, &["rev-parse", "HEAD"]);
    for name in names {
        // Confirm git refuses to create it, then plant the loose file.
        let refused = Command::new("git")
            .arg("-C")
            .arg(&dir.path)
            .args(["update-ref", name, &oid])
            .output()
            .expect("run git update-ref");
        assert!(!refused.status.success(), "git created {name}");
        let path = dir.path.join(".git").join(name);
        std::fs::write(&path, format!("{oid}\n")).expect("plant broken ref");
    }
    (dir, oid)
}

const BROKEN: &[&str] = &["refs/heads/broken...ref", "refs/heads/x:y"];

#[test]
fn delete_ref_removes_broken_loose_refs() {
    let (dir, _oid) = repo_with_broken_refs("single", BROKEN);
    let repo = Repository::open(dir.path.join(".git")).expect("open repo");
    for name in BROKEN {
        repo.delete_ref(DeleteRef {
            name: DeleteRefName::new(name).expect("delete name"),
            expected_old: None,
            expected: None,
            reflog: None,
            reflog_committer: None,
        })
        .unwrap_or_else(|err| panic!("delete {name}: {err}"));
        assert!(
            !dir.path.join(".git").join(name).exists(),
            "{name} still on disk"
        );
    }
}

#[test]
fn apply_ref_batch_removes_broken_loose_refs() {
    let (dir, _oid) = repo_with_broken_refs("batch", BROKEN);
    let repo = Repository::open(dir.path.join(".git")).expect("open repo");
    let changes: Vec<RefBatchChange> = BROKEN
        .iter()
        .map(|name| {
            RefBatchChange::Delete(DeleteRef {
                name: DeleteRefName::new(name).expect("delete name"),
                expected_old: None,
                expected: None,
                reflog: None,
                reflog_committer: None,
            })
        })
        .collect();
    repo.apply_ref_batch(&changes).expect("batch delete");
    for name in BROKEN {
        assert!(
            !dir.path.join(".git").join(name).exists(),
            "{name} still on disk"
        );
    }
}

#[test]
fn broken_names_are_not_full_names() {
    // The broken names stay invalid for create/update.
    for name in BROKEN {
        assert!(FullName::new(name).is_err(), "{name} accepted as FullName");
    }
}

#[test]
fn delete_ref_name_rejects_what_git_refuses_to_delete() {
    // `git update-ref -d` refuses these with "refusing to update ref with bad
    // name": they escape `refs/`, are not normalised, or are one-level names
    // that are not pseudo-ref shaped.
    for name in [
        "",
        "refs/",
        "refs/heads/",
        "refs/heads//x",
        "refs/heads/./x",
        "refs/heads/a/../b",
        "refs/../config",
        "my-file",
        "-x",
    ] {
        assert!(DeleteRefName::new(name).is_err(), "{name:?} accepted");
    }
    for name in ["HEAD", "ORIG_HEAD", "refs/heads/main", "refs/heads/a..b"] {
        assert!(DeleteRefName::new(name).is_ok(), "{name:?} rejected");
    }
}

#[test]
fn find_reference_errors_on_a_broken_name_that_exists_on_disk() {
    let (dir, _oid) = repo_with_broken_refs("find", BROKEN);
    let repo = Repository::open(dir.path.join(".git")).expect("open repo");
    for name in BROKEN {
        assert!(dir.path.join(".git").join(name).exists());
        assert!(repo.find_reference(name).is_err(), "{name} did not error");
    }
}
