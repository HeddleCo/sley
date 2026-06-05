//! Differential interop tests for `git verify-commit` vs the system git binary.
//!
//! Each test builds a temp repository with the real `git` binary, then runs the
//! same `verify-commit` invocation through both `git` and `sley` and asserts
//! that stdout, stderr, and the exit code match byte-for-byte. Because both
//! binaries see the same objects built under a fixed identity/date environment,
//! commit/tree/blob object names are identical and can be compared directly.
//!
//! The cases here are deliberately limited to behavior that does *not* depend on
//! GnuPG: unsigned commits (which git reports as unverifiable with no output),
//! non-commit objects, unresolvable arguments, and option/usage handling. Signed
//! commits would require git to shell out to `gpg`, whose presence and output are
//! environment-specific, so they are out of scope for a hermetic differential
//! test. The whole file is gated on `git --version` succeeding, so it is a no-op
//! where git is absent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide monotonic counter that disambiguates temp directories created by
/// different test threads. `libtest` runs tests in parallel, so two threads can
/// observe the same `pid` and the same nanosecond clock reading; without a
/// per-call serial the directory names collide and a second `git init` lands in a
/// half-built repo (surfacing as `fatal: cannot mkdir …/repo: File exists`).
static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let serial = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "sley-{name}-{}-{nanos}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// The fixed identity/date environment the task pins, applied to every command
/// (both `git` and `sley`) so commit/tag object ids are reproducible.
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

/// Run a repository-building `git` command at a specific author/committer date so
/// commits and annotated tags get distinct, deterministic timestamps.
fn git_at(cwd: &Path, args: &[&str], date: &str) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
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

/// Capture the trimmed stdout of a `git` command (e.g. `rev-parse`), aborting on
/// failure.
fn git_capture(cwd: &Path, args: &[&str]) -> String {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 output")
        .trim()
        .to_string()
}

/// Assert git and sley produce byte-identical stdout, identical stderr, and the
/// same exit code for `args` run in `cwd`.
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
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {args:?}"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit code differs for {args:?}\nsley stdout: {}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr),
    );
}

fn write_commit(repo: &Path, file: &str, contents: &str, message: &str, date: &str) {
    fs::write(repo.join(file), contents).unwrap_or_else(|err| panic!("write {file}: {err}"));
    git_ok(repo, &["add", file]);
    git_at(repo, &["commit", "-q", "-m", message], date);
}

/// A small repository with two unsigned commits, plus lightweight and annotated
/// tags, so non-commit object handling can be exercised:
///
/// ```text
/// c1 (annotated tag: anno-tag) - c2 (HEAD, lightweight tag: light-tag)
/// ```
fn build_repo() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("verify-commit");
    let repo = root.join("repo");
    git_ok(
        &root,
        &["init", "-q", "-b", "main", repo.to_str().expect("utf8")],
    );

    write_commit(&repo, "a.txt", "a\n", "first commit", "@1790000000 -0500");
    git_at(
        &repo,
        &["tag", "-a", "-m", "annotated", "anno-tag"],
        "@1790000100 -0500",
    );

    write_commit(&repo, "b.txt", "b\n", "second commit", "@1790000200 -0500");
    git_ok(&repo, &["tag", "light-tag"]);

    (root, repo)
}

#[test]
fn verify_commit_unsigned_commits_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // Unsigned commits cannot be verified: git prints nothing and exits 1, even
    // under -v/--raw. Exercise refs, suffix navigation, and full/short OIDs.
    let head = git_capture(&repo, &["rev-parse", "HEAD"]);
    let parent = git_capture(&repo, &["rev-parse", "HEAD~1"]);
    let head_short = git_capture(&repo, &["rev-parse", "--short", "HEAD"]);

    for args in [
        vec!["verify-commit", "HEAD"],
        vec!["verify-commit", "HEAD~1"],
        vec!["verify-commit", "-v", "HEAD"],
        vec!["verify-commit", "--verbose", "HEAD"],
        vec!["verify-commit", "--raw", "HEAD"],
        vec!["verify-commit", "-v", "--raw", "HEAD"],
        vec!["verify-commit", "main"],
        vec!["verify-commit", head.as_str()],
        vec!["verify-commit", parent.as_str()],
        vec!["verify-commit", head_short.as_str()],
        // Multiple unsigned commits: still silent, still exit 1.
        vec!["verify-commit", "HEAD", "HEAD~1"],
        vec!["verify-commit", "-v", "HEAD", "HEAD~1"],
        // `--` terminates option parsing.
        vec!["verify-commit", "--", "HEAD"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_commit_non_commit_objects_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    let tree = git_capture(&repo, &["rev-parse", "HEAD^{tree}"]);
    let blob = git_capture(&repo, &["rev-parse", "HEAD:a.txt"]);
    let anno_oid = git_capture(&repo, &["rev-parse", "anno-tag"]);

    for args in [
        // A tree OID: cannot verify a non-commit object of type tree.
        vec!["verify-commit", tree.as_str()],
        vec!["verify-commit", "-v", tree.as_str()],
        // A blob OID: type blob.
        vec!["verify-commit", blob.as_str()],
        // `verify-commit` does not peel tags: an annotated tag is type tag,
        // whether referenced by name or by OID. The error echoes the argument.
        vec!["verify-commit", "anno-tag"],
        vec!["verify-commit", anno_oid.as_str()],
        vec!["verify-commit", "-v", "anno-tag"],
        // A lightweight tag points straight at the commit, so it verifies like
        // the commit (unsigned -> silent, exit 1).
        vec!["verify-commit", "light-tag"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_commit_unresolvable_arguments_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    for args in [
        vec!["verify-commit", "no-such-ref"],
        // Short hex prefix that resolves to nothing: "commit '..' not found.".
        vec!["verify-commit", "1234"],
        // A full-length but absent OID parses, so the object read fails instead:
        // git reports "unable to read file." echoing the argument as given.
        vec!["verify-commit", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"],
        // All arguments are processed; two bad refs report two errors, exit 1.
        vec!["verify-commit", "nope-one", "nope-two"],
        // A bad ref followed by a good (unsigned) commit: bad reports, good is
        // silent, overall exit 1.
        vec!["verify-commit", "nope", "HEAD"],
        // A non-commit followed by a bad ref: both report, in order.
        vec!["verify-commit", "HEAD^{tree}", "nope"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_commit_usage_and_option_errors_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    for args in [
        // No commit-ish: usage error, exit 129.
        vec!["verify-commit"],
        // Unknown long option: error + usage, exit 129.
        vec!["verify-commit", "--bogus"],
        vec!["verify-commit", "--bogus", "HEAD"],
        // Unknown short switch: error + usage, exit 129.
        vec!["verify-commit", "-z"],
        vec!["verify-commit", "-z", "HEAD"],
        // `-h` prints usage to stdout and exits 129. (`--help` is excluded: real
        // git execs the man page, which is not reproducible in a hermetic test.)
        vec!["verify-commit", "-h"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}
