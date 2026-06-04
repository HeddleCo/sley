//! Differential interop tests for `git blame` vs the system git binary.
//!
//! Each test builds a temp repository with the real `git` binary, then runs
//! the same `blame` invocation through both `git` and `git-rs` and asserts the
//! stdout, stderr, and exit code match. The whole file is gated on `git
//! --version` succeeding so it is a no-op where git is unavailable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// Run `program` with the fixed identity/date environment the task pins so that
/// commit object ids are reproducible across machines. The same environment is
/// used for both `git` and `git-rs`.
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

/// Run a repository-building `git` command, optionally overriding the author
/// name/email and author/committer dates so blame output exercises varying
/// metadata. A failure aborts the test.
fn git_commit_env(cwd: &Path, args: &[&str], author_name: &str, author_email: &str, date: &str) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", author_name)
        .env("GIT_COMMITTER_EMAIL", author_email)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    run_env(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Assert git and git-rs produce byte-identical stdout, identical stderr, and
/// the same exit code for `args` run in `cwd`.
fn assert_same(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for {args:?}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {args:?}"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}"
    );
}

/// Build a repository whose `f.txt` is touched by three commits with two
/// different authors and timestamps, so blame must attribute lines to the
/// correct commit, mark the root commit as a boundary, and pad the author and
/// line-number columns. Returns the repo path.
fn build_history_repo() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("blame-history");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("utf8 path"),
        ],
    );

    // c1 (root): three lines, author Tester.
    fs::write(repo.join("f.txt"), "line one\nline two\nline three\n").expect("write f.txt c1");
    git_ok(&repo, &["add", "f.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "c1"],
        "Tester",
        "tester@example.com",
        "@1790000000 -0500",
    );

    // c2: change line two, append line four. Same author, later date.
    fs::write(
        repo.join("f.txt"),
        "line one\nline two changed\nline three\nline four\n",
    )
    .expect("write f.txt c2");
    git_ok(&repo, &["add", "f.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "c2"],
        "Tester",
        "tester@example.com",
        "@1790005000 -0500",
    );

    // c3: append more lines with a longer author name to force column padding.
    fs::write(
        repo.join("f.txt"),
        "line one\nline two changed\nline three\nline four\nl5\nl6\nl7\nl8\nl9\nl10\nl11\n",
    )
    .expect("write f.txt c3");
    git_ok(&repo, &["add", "f.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "c3"],
        "A Longer Name",
        "longer@example.com",
        "@1790010000 -0500",
    );

    (root, repo)
}

/// Default-format blame plus the supported display flags must all match git on
/// a multi-commit, multi-author history.
#[test]
fn blame_default_and_flags_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_history_repo();

    assert_same(&repo, &["blame", "f.txt"]);
    assert_same(&repo, &["blame", "-l", "f.txt"]);
    assert_same(&repo, &["blame", "-s", "f.txt"]);
    assert_same(&repo, &["blame", "-e", "f.txt"]);
    assert_same(&repo, &["blame", "-t", "f.txt"]);
    assert_same(&repo, &["blame", "-l", "-t", "f.txt"]);
    assert_same(&repo, &["blame", "-e", "-s", "f.txt"]);
    assert_same(&repo, &["blame", "--root", "f.txt"]);
    assert_same(&repo, &["blame", "--abbrev=10", "f.txt"]);
    assert_same(&repo, &["blame", "--abbrev=4", "f.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// The `-L` line range, in its several spellings, must match git including the
/// column widths derived from the displayed range.
#[test]
fn blame_line_ranges_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_history_repo();

    assert_same(&repo, &["blame", "-L", "2,3", "f.txt"]);
    assert_same(&repo, &["blame", "-L", "2,2", "f.txt"]);
    assert_same(&repo, &["blame", "-L", "9", "f.txt"]);
    assert_same(&repo, &["blame", "-L", ",3", "f.txt"]);
    assert_same(&repo, &["blame", "-L", "2,+2", "f.txt"]);
    assert_same(&repo, &["blame", "-L4,5", "f.txt"]);
    // Two ranges combine into the union of displayed lines.
    assert_same(&repo, &["blame", "-L", "1,1", "-L", "3,3", "f.txt"]);
    // start > end (both valid) prints nothing.
    assert_same(&repo, &["blame", "-L", "5,2", "f.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// `-L` error cases (out of range, zero) must match git's message and exit
/// code.
#[test]
fn blame_line_range_errors_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_history_repo();

    assert_same(&repo, &["blame", "-L", "100,200", "f.txt"]);
    assert_same(&repo, &["blame", "-L", "0", "f.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// Blaming an explicit revision (and a revision with `--`) must match git, and
/// the root commit is still the only boundary.
#[test]
fn blame_explicit_revision_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_history_repo();

    assert_same(&repo, &["blame", "HEAD~1", "--", "f.txt"]);
    assert_same(&repo, &["blame", "HEAD~2", "f.txt"]);
    assert_same(&repo, &["blame", "HEAD", "f.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// A file created in a non-root commit is attributed to its creating commit but
/// that commit is NOT a boundary (only true roots are). Verify against git.
#[test]
fn blame_file_added_later_is_not_boundary() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("blame-added");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("utf8 path"),
        ],
    );
    fs::write(repo.join("other.txt"), "other\n").expect("write other.txt");
    git_ok(&repo, &["add", "other.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "root"],
        "Tester",
        "tester@example.com",
        "@1790000000 -0500",
    );
    fs::write(repo.join("new.txt"), "created a\ncreated b\n").expect("write new.txt");
    git_ok(&repo, &["add", "new.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "addnew"],
        "Tester",
        "tester@example.com",
        "@1790005000 -0500",
    );

    assert_same(&repo, &["blame", "new.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// Blame must work for a path inside a subdirectory and when invoked from
/// within that subdirectory (prefix handling).
#[test]
fn blame_subdirectory_path_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("blame-subdir");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("utf8 path"),
        ],
    );
    fs::create_dir_all(repo.join("dir")).expect("create dir");
    fs::write(repo.join("dir/nested.txt"), "alpha\nbeta\n").expect("write nested c1");
    git_ok(&repo, &["add", "dir/nested.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "c1"],
        "Tester",
        "tester@example.com",
        "@1790000000 -0500",
    );
    fs::write(repo.join("dir/nested.txt"), "alpha\nbeta changed\ngamma\n")
        .expect("write nested c2");
    git_ok(&repo, &["add", "dir/nested.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "c2"],
        "Tester",
        "tester@example.com",
        "@1790005000 -0500",
    );

    // From the repo root with a path that includes the directory.
    assert_same(&repo, &["blame", "dir/nested.txt"]);
    // From inside the subdirectory with a bare filename (prefix resolution).
    let dir = repo.join("dir");
    assert_same(&dir, &["blame", "nested.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// An empty file produces no output and exits 0, matching git.
#[test]
fn blame_empty_file_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("blame-empty");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("utf8 path"),
        ],
    );
    fs::write(repo.join("empty.txt"), "").expect("write empty.txt");
    git_ok(&repo, &["add", "empty.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "e"],
        "Tester",
        "tester@example.com",
        "@1790000000 -0500",
    );

    assert_same(&repo, &["blame", "empty.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// A final line without a trailing newline still renders one blame entry, and
/// a CRLF line keeps its carriage return; both must match git byte-for-byte.
#[test]
fn blame_newline_edge_cases_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("blame-newline");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("utf8 path"),
        ],
    );
    // No trailing newline on the last line.
    fs::write(repo.join("nonl.txt"), "first\nno newline").expect("write nonl.txt");
    // A CRLF-terminated file (committed with autocrlf off, the default here).
    fs::write(repo.join("crlf.txt"), "crlf line\r\nsecond\r\n").expect("write crlf.txt");
    git_ok(&repo, &["add", "nonl.txt", "crlf.txt"]);
    git_commit_env(
        &repo,
        &["commit", "-q", "-m", "edge"],
        "Tester",
        "tester@example.com",
        "@1790000000 -0500",
    );

    assert_same(&repo, &["blame", "nonl.txt"]);
    assert_same(&repo, &["blame", "crlf.txt"]);

    fs::remove_dir_all(&root).ok();
}

/// Blaming a path that does not exist reports git's fatal message and exit 128.
#[test]
fn blame_missing_path_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_history_repo();

    assert_same(&repo, &["blame", "does-not-exist.txt"]);

    fs::remove_dir_all(&root).ok();
}
