//! Differential interop tests for `git cherry-pick` and `git revert` against the
//! system `git` binary. Fixtures are built with real `git` (fixed identity +
//! dates → deterministic oids), copied, then the operation runs in each copy and
//! the substantive results are compared (commit oid, worktree bytes, index
//! conflict stages, state files, exit codes).

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

fn sley(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::sley_bin!(), cwd, args)
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

fn default_branch(dir: &Path) -> String {
    for name in ["main", "master"] {
        if git(dir, &["rev-parse", "--verify", name]).status.success() {
            return name.to_string();
        }
    }
    "main".to_string()
}

#[test]
fn cherry_pick_clean_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("cp-clean");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "f.txt", "1\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    git_ok(&reference, &["checkout", "-q", "-b", "topic"]);
    write_file(&reference, "g.txt", "g\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "add g"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    write_file(&reference, "f.txt", "1\nmain\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "mainwork"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["cherry-pick", "topic"]);
    let rs_out = sley(&candidate, &["cherry-pick", "topic"]);
    assert!(ref_out.status.success(), "git cherry-pick failed");
    assert!(
        rs_out.status.success(),
        "sley cherry-pick failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // Picked commit preserves author, sets new committer → identical oid.
    assert_eq!(
        head(&candidate),
        head(&reference),
        "cherry-pick oid differs"
    );
    assert!(candidate.join("g.txt").exists(), "g.txt not applied");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn cherry_pick_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("cp-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "x.txt", "1\n2\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    git_ok(&reference, &["checkout", "-q", "-b", "topic"]);
    write_file(&reference, "x.txt", "1\nTOPIC\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "topicwork"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    write_file(&reference, "x.txt", "1\nMAIN\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "mainwork"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["cherry-pick", "topic"]);
    let rs_out = sley(&candidate, &["cherry-pick", "topic"]);
    assert_eq!(ref_out.status.code(), Some(1));
    assert_eq!(
        rs_out.status.code(),
        Some(1),
        "sley cherry-pick conflict exit differs: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // Conflict markers (incl. the "<short> (subject)" label) match git exactly.
    assert_eq!(
        fs::read(candidate.join("x.txt")).expect("test operation should succeed"),
        fs::read(reference.join("x.txt")).expect("test operation should succeed"),
        "cherry-pick conflict markers differ"
    );
    assert_eq!(
        git(&candidate, &["ls-files", "-u"]).stdout,
        git(&reference, &["ls-files", "-u"]).stdout,
        "cherry-pick index stages differ"
    );
    assert!(
        git(&candidate, &["rev-parse", "--verify", "CHERRY_PICK_HEAD"])
            .status
            .success(),
        "CHERRY_PICK_HEAD not recorded"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn cherry_pick_abort_restores_state() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("cp-abort");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "x.txt", "1\n2\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["checkout", "-q", "-b", "topic"]);
    write_file(&repo, "x.txt", "1\nTOPIC\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "topicwork"]);
    let default = default_branch(&repo);
    git_ok(&repo, &["checkout", "-q", &default]);
    write_file(&repo, "x.txt", "1\nMAIN\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "mainwork"]);
    let pre = head(&repo);

    assert_eq!(
        sley(&repo, &["cherry-pick", "topic"]).status.code(),
        Some(1)
    );
    let abort = sley(&repo, &["cherry-pick", "--abort"]);
    assert!(
        abort.status.success(),
        "cherry-pick --abort failed: {}",
        String::from_utf8_lossy(&abort.stderr)
    );
    assert_eq!(head(&repo), pre, "HEAD moved after abort");
    assert_eq!(
        fs::read(repo.join("x.txt")).expect("test operation should succeed"),
        b"1\nMAIN\n3\n"
    );
    assert!(
        !git(&repo, &["rev-parse", "--verify", "CHERRY_PICK_HEAD"])
            .status
            .success()
    );
    assert!(git(&repo, &["ls-files", "-u"]).stdout.is_empty());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn revert_clean_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("revert-clean");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "f.txt", "1\n2\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    write_file(&reference, "f.txt", "1\nCHANGED\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "change"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["revert", "--no-edit", "HEAD"]);
    let rs_out = sley(&candidate, &["revert", "--no-edit", "HEAD"]);
    assert!(ref_out.status.success(), "git revert failed");
    assert!(
        rs_out.status.success(),
        "sley revert failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // Revert message + reverted tree → identical commit oid.
    assert_eq!(head(&candidate), head(&reference), "revert oid differs");
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        b"1\n2\n3\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn revert_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("revert-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "x.txt", "1\n2\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    write_file(&reference, "x.txt", "1\nB\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "b"]);
    write_file(&reference, "x.txt", "1\nC\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "c"]);
    copy_dir_all(&reference, &candidate);

    // Reverting the middle commit conflicts with the current content.
    let ref_out = git(&reference, &["revert", "--no-edit", "HEAD~1"]);
    let rs_out = sley(&candidate, &["revert", "--no-edit", "HEAD~1"]);
    assert_eq!(ref_out.status.code(), Some(1));
    assert_eq!(
        rs_out.status.code(),
        Some(1),
        "sley revert conflict exit differs: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    assert_eq!(
        fs::read(candidate.join("x.txt")).expect("test operation should succeed"),
        fs::read(reference.join("x.txt")).expect("test operation should succeed"),
        "revert conflict markers differ"
    );
    assert_eq!(
        git(&candidate, &["ls-files", "-u"]).stdout,
        git(&reference, &["ls-files", "-u"]).stdout,
        "revert index stages differ"
    );
    assert!(
        git(&candidate, &["rev-parse", "--verify", "REVERT_HEAD"])
            .status
            .success(),
        "REVERT_HEAD not recorded"
    );

    fs::remove_dir_all(&root).ok();
}
