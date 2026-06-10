//! Differential interop tests for `git merge` against the system `git` binary.
//!
//! Strategy: build a fixture with real `git` (fixed identity + dates so object
//! ids are deterministic), copy the repo, then run `sley merge` in one copy and
//! real `git merge` in the other and compare the substantive results (merge
//! commit oid, worktree bytes, index conflict stages, exit codes). git's merge
//! stdout includes a diffstat that sley does not emit yet, so stdout is checked
//! loosely (contains) rather than byte-for-byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// Run a program with a fixed, deterministic git identity + timestamp so commit
/// object ids are reproducible across `git` and `sley`.
fn run_env(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_env(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write_file(cwd: &Path, name: &str, content: &str) {
    fs::write(cwd.join(name), content).expect("write file");
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            copy_dir_all(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy file");
        }
    }
}

fn head(cwd: &Path) -> String {
    String::from_utf8_lossy(&git(cwd, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string()
}

/// Build a repo on the default branch with two divergent branches:
/// base has a.txt + b.txt; `feature` changes b.txt; default branch changes a.txt
/// (non-overlapping → clean 3-way merge).
fn setup_clean(dir: &Path) {
    git_ok(
        dir.parent().unwrap_or(dir),
        &[
            "init",
            "-q",
            dir.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(dir, "a.txt", "a1\na2\na3\n");
    write_file(dir, "b.txt", "b1\nb2\nb3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "base"]);
    git_ok(dir, &["checkout", "-q", "-b", "feature"]);
    write_file(dir, "b.txt", "b1\nBETA\nb3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "feat"]);
    // back to the original branch (main or master)
    let default = default_branch(dir);
    git_ok(dir, &["checkout", "-q", &default]);
    write_file(dir, "a.txt", "a1\nALPHA\na3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "mainwork"]);
}

fn default_branch(dir: &Path) -> String {
    // Whichever of main/master currently has commits.
    for name in ["main", "master"] {
        if git(dir, &["rev-parse", "--verify", name]).status.success() {
            return name.to_string();
        }
    }
    "main".to_string()
}

#[test]
fn merge_clean_threeway_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-clean");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    setup_clean(&reference);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(
        &reference,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );
    let rs_out = git_rs(
        &candidate,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );

    assert!(ref_out.status.success(), "git merge failed");
    assert!(
        rs_out.status.success(),
        "sley merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // The merge commit object must be byte-identical (same tree, parents,
    // identity, message) → identical oid.
    assert_eq!(
        head(&candidate),
        head(&reference),
        "merge commit oid differs from git"
    );
    // Two parents, second is the feature tip.
    assert_eq!(
        git(&candidate, &["rev-parse", "HEAD^2"]).stdout,
        git(&reference, &["rev-parse", "HEAD^2"]).stdout
    );
    // Worktree content matches.
    assert_eq!(
        fs::read(candidate.join("a.txt")).expect("test operation should succeed"),
        fs::read(reference.join("a.txt")).expect("test operation should succeed")
    );
    assert_eq!(
        fs::read(candidate.join("b.txt")).expect("test operation should succeed"),
        fs::read(reference.join("b.txt")).expect("test operation should succeed")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_fast_forward_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-ff");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "f.txt", "one\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "c1"]);
    git_ok(&reference, &["checkout", "-q", "-b", "feature"]);
    write_file(&reference, "f.txt", "one\ntwo\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "c2"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "feature"]);
    let rs_out = git_rs(&candidate, &["merge", "feature"]);
    assert!(ref_out.status.success());
    assert!(
        rs_out.status.success(),
        "sley ff merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    assert_eq!(
        head(&candidate),
        head(&reference),
        "ff HEAD differs from git"
    );
    assert!(
        String::from_utf8_lossy(&rs_out.stdout).contains("Fast-forward"),
        "expected Fast-forward in output, got: {}",
        String::from_utf8_lossy(&rs_out.stdout)
    );
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        fs::read(reference.join("f.txt")).expect("test operation should succeed")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_already_up_to_date_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-utd");
    let repo = root.join("repo");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "one\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "c1"]);
    git_ok(&repo, &["checkout", "-q", "-b", "old", "HEAD"]);
    let default = default_branch(&repo);
    git_ok(&repo, &["checkout", "-q", &default]);
    write_file(&repo, "f.txt", "one\ntwo\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "c2"]);

    // Merging an ancestor ("old") is a no-op.
    let rs_out = git_rs(&repo, &["merge", "old"]);
    assert!(rs_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rs_out.stdout).trim(),
        "Already up to date."
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "x.txt", "1\n2\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    git_ok(&reference, &["checkout", "-q", "-b", "feature"]);
    write_file(&reference, "x.txt", "1\nFEATURE\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "feat"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    write_file(&reference, "x.txt", "1\nMAIN\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "mainwork"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "-m", "M", "feature"]);
    let rs_out = git_rs(&candidate, &["merge", "-m", "M", "feature"]);

    // Both fail with the same conflict exit code.
    assert_eq!(ref_out.status.code(), Some(1));
    assert_eq!(
        rs_out.status.code(),
        Some(1),
        "sley merge conflict exit differs: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // Conflicted working-tree bytes (markers) must match git exactly.
    assert_eq!(
        fs::read(candidate.join("x.txt")).expect("test operation should succeed"),
        fs::read(reference.join("x.txt")).expect("test operation should succeed"),
        "conflict markers differ from git"
    );
    // Index conflict stages must match git.
    assert_eq!(
        git(&candidate, &["ls-files", "-u"]).stdout,
        git(&reference, &["ls-files", "-u"]).stdout,
        "unmerged index stages differ from git"
    );
    // MERGE_HEAD recorded.
    assert!(
        git(&candidate, &["rev-parse", "--verify", "MERGE_HEAD"])
            .status
            .success(),
        "sley did not record MERGE_HEAD"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_abort_restores_pre_merge_state() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-abort");
    let repo = root.join("repo");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "x.txt", "1\n2\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["checkout", "-q", "-b", "feature"]);
    write_file(&repo, "x.txt", "1\nFEATURE\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "feat"]);
    let default = default_branch(&repo);
    git_ok(&repo, &["checkout", "-q", &default]);
    write_file(&repo, "x.txt", "1\nMAIN\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "mainwork"]);
    let pre_head = head(&repo);

    let conflict = git_rs(&repo, &["merge", "-m", "M", "feature"]);
    assert_eq!(conflict.status.code(), Some(1));

    let abort = git_rs(&repo, &["merge", "--abort"]);
    assert!(
        abort.status.success(),
        "sley merge --abort failed: {}",
        String::from_utf8_lossy(&abort.stderr)
    );
    assert_eq!(head(&repo), pre_head, "HEAD moved after abort");
    // Working tree restored to ours; no merge state left.
    assert_eq!(
        fs::read(repo.join("x.txt")).expect("test operation should succeed"),
        b"1\nMAIN\n3\n"
    );
    assert!(
        !git(&repo, &["rev-parse", "--verify", "MERGE_HEAD"])
            .status
            .success(),
        "MERGE_HEAD still present after abort"
    );
    // Index is clean (no unmerged entries).
    assert!(
        git(&repo, &["ls-files", "-u"]).stdout.is_empty(),
        "unmerged entries remain after abort"
    );

    fs::remove_dir_all(&root).ok();
}
