//! Differential interop tests for `git apply`, `git gc`, and `git repack`
//! against the system `git` binary.

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
    run_env("git", cwd, args)
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
    Command::new("git")
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

#[test]
fn apply_modifies_worktree_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-mod");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "a\nb\nc\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    write_file(&repo, "f.txt", "a\nB\nc\n");
    let patch = git(&repo, &["diff"]).stdout;
    git_ok(&repo, &["checkout", "--", "f.txt"]);
    let patch_path = root.join("change.patch");
    fs::write(&patch_path, &patch).expect("write patch");
    let patch_arg = patch_path.to_str().expect("test operation should succeed");

    let candidate = root.join("candidate");
    let reference = root.join("reference");
    copy_dir_all(&repo, &candidate);
    copy_dir_all(&repo, &reference);

    let rs = git_rs(&candidate, &["apply", patch_arg]);
    assert!(
        rs.status.success(),
        "sley apply failed: {}",
        String::from_utf8_lossy(&rs.stderr)
    );
    git_ok(&reference, &["apply", patch_arg]);
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        fs::read(reference.join("f.txt")).expect("test operation should succeed"),
        "sley apply produced different bytes than git apply"
    );
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        b"a\nB\nc\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_check_succeeds_for_clean_patch() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-check");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "1\n2\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    write_file(&repo, "f.txt", "1\n2\nthree\n");
    let patch = git(&repo, &["diff"]).stdout;
    git_ok(&repo, &["checkout", "--", "f.txt"]);
    let patch_path = root.join("c.patch");
    fs::write(&patch_path, &patch).expect("write patch");

    // --check must not modify the worktree and must succeed for an applicable patch.
    let out = git_rs(
        &repo,
        &[
            "apply",
            "--check",
            patch_path.to_str().expect("test operation should succeed"),
        ],
    );
    assert!(
        out.status.success(),
        "sley apply --check rejected a clean patch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(repo.join("f.txt")).expect("test operation should succeed"),
        b"1\n2\n3\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_creates_new_file_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-new");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "seed.txt", "seed\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Stage a new file, capture its creation patch, then unstage/remove it.
    write_file(&repo, "added.txt", "hello\nworld\n");
    git_ok(&repo, &["add", "added.txt"]);
    let patch = git(&repo, &["diff", "--cached"]).stdout;
    git_ok(&repo, &["rm", "-f", "--quiet", "added.txt"]);
    let patch_path = root.join("new.patch");
    fs::write(&patch_path, &patch).expect("write patch");

    let out = git_rs(
        &repo,
        &[
            "apply",
            patch_path.to_str().expect("test operation should succeed"),
        ],
    );
    assert!(
        out.status.success(),
        "sley apply (new file) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(repo.join("added.txt")).expect("test operation should succeed"),
        b"hello\nworld\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn gc_consolidates_loose_objects_and_stays_valid() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("gc");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    for i in 0..3 {
        write_file(&repo, "f.txt", &format!("v{i}\n"));
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-qm", &format!("c{i}")]);
    }
    let head_before = git(&repo, &["rev-parse", "HEAD"]).stdout;

    let out = git_rs(&repo, &["gc"]);
    assert!(
        out.status.success(),
        "sley gc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A pack now exists.
    let pack_dir = repo.join(".git/objects/pack");
    let has_pack = fs::read_dir(&pack_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "pack").unwrap_or(false))
        })
        .unwrap_or(false);
    assert!(has_pack, "sley gc did not produce a pack");

    // Repo is still valid and complete according to upstream git.
    let fsck = git(&repo, &["fsck", "--no-progress"]);
    assert!(
        fsck.status.success(),
        "git fsck failed after sley gc: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]).stdout, head_before);
    let log_lines = String::from_utf8_lossy(&git(&repo, &["log", "--oneline"]).stdout)
        .lines()
        .count();
    assert_eq!(log_lines, 3, "history not fully readable after gc");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn repack_d_keeps_repository_complete() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("repack");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    for i in 0..4 {
        write_file(&repo, "f.txt", &format!("line {i}\ncommon\n"));
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-qm", &format!("c{i}")]);
    }
    let out = git_rs(&repo, &["repack", "-d"]);
    assert!(
        out.status.success(),
        "sley repack -d failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fsck = git(&repo, &["fsck", "--no-progress"]);
    assert!(
        fsck.status.success(),
        "git fsck failed after sley repack -d: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert!(
        git(&repo, &["cat-file", "-e", "HEAD^{tree}"])
            .status
            .success()
    );

    fs::remove_dir_all(&root).ok();
}
