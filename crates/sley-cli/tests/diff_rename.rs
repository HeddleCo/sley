//! Differential interop tests for diff inexact rename/copy detection vs git.

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
    assert!(git(cwd, args).status.success(), "git {args:?} failed");
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

fn assert_same(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}"
    );
}

/// A renamed file with a small edit should be reported as `R<score>` (3-digit,
/// zero-padded) by default and with `-M`, and as delete+add with `--no-renames`.
#[test]
fn diff_inexact_rename_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-rename");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::write(repo.join("orig.txt"), "a\nb\nc\nd\ne\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["mv", "orig.txt", "renamed.txt"]);
    fs::write(repo.join("renamed.txt"), "a\nb\nC\nd\ne\n").unwrap();
    git_ok(&repo, &["add", "renamed.txt"]);

    // Default (git detects renames via diff.renames), explicit -M, a threshold,
    // and --no-renames must all match git.
    assert_same(&repo, &["diff", "--cached", "--name-status"]);
    assert_same(&repo, &["diff", "--cached", "-M", "--name-status"]);
    assert_same(&repo, &["diff", "--cached", "-M50", "--name-status"]);
    assert_same(
        &repo,
        &["diff", "--cached", "--no-renames", "--name-status"],
    );

    fs::remove_dir_all(&root).ok();
}

/// One edited line of three: git reports `R066` / `similarity index 66%`, not 67.
/// The similarity percentage must match git's MAX_SCORE integer truncation.
#[test]
fn diff_inexact_rename_one_edit_similarity_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-rename-sim");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::write(repo.join("f.txt"), "a\nb\nc\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["mv", "f.txt", "g.txt"]);
    fs::write(repo.join("g.txt"), "a\nB\nc\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "rename"]);

    // R066 in --name-status and the `similarity index 66%` header in the patch.
    assert_same(
        &repo,
        &["diff-tree", "-M", "--name-status", "HEAD~1", "HEAD"],
    );
    assert_same(&repo, &["diff-tree", "-M", "-p", "HEAD~1", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

/// A binary file whose content is unchanged (a pure mode change) renders in
/// `--stat` as just `Bin`, with no ` N -> M bytes` suffix -- matching git.
#[cfg(unix)]
#[test]
fn diff_binary_mode_change_stat_matches_git() {
    use std::os::unix::fs::PermissionsExt;
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-bin-stat");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::write(repo.join("data.bin"), b"bin\x00data\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "add"]);
    let file = repo.join("data.bin");
    let mut perms = fs::metadata(&file).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&file, perms).unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "chmod"]);

    // Content identical on both sides -> git prints " data.bin | Bin".
    assert_same(&repo, &["diff-tree", "--stat", "--no-commit-id", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}
