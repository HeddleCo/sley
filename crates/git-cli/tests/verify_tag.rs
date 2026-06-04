//! Differential interop tests for `git verify-tag` vs the system git binary.
//!
//! Each test builds a temp repository with the real `git` binary, then runs the
//! same `verify-tag` invocation through both `git` and `git-rs` and asserts that
//! stdout, stderr, and the exit code match byte-for-byte. Because both binaries
//! see the same objects built under a fixed identity/date environment,
//! tag/commit/tree/blob object names are identical and can be compared directly.
//!
//! The cases here are deliberately limited to behavior that does *not* depend on
//! a signature backend: unsigned annotated tags (which git reports with
//! `error: no signature found`, echoing the tag body under `-v`), non-tag objects
//! (including a lightweight tag's commit target, since `verify-tag` does not
//! peel), unresolvable arguments, and option/usage handling. Signed tags would
//! require git to shell out to `gpg`/`gpgsm`/`ssh-keygen`, whose presence and
//! output are environment-specific, so they are out of scope for a hermetic
//! differential test. The whole file is gated on `git --version` succeeding, so
//! it is a no-op where git is absent.

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
        "git-rs-{name}-{}-{nanos}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// The fixed identity/date environment the task pins, applied to every command
/// (both `git` and `git-rs`) so commit/tag object ids are reproducible.
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
    run_env(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
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

/// Assert git and git-rs produce byte-identical stdout, identical stderr, and the
/// same exit code for `args` run in `cwd`.
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
        "exit code differs for {args:?}\ngit-rs stdout: {}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr),
    );
}

fn write_commit(repo: &Path, file: &str, contents: &str, message: &str, date: &str) {
    fs::write(repo.join(file), contents).unwrap_or_else(|err| panic!("write {file}: {err}"));
    git_ok(repo, &["add", file]);
    git_at(repo, &["commit", "-q", "-m", message], date);
}

/// A small repository with two unsigned commits and a spread of tag kinds, so
/// every `verify-tag` code path can be exercised against gpg-independent objects:
///
/// ```text
/// c1 (annotated tag: anno-tag) - c2 (HEAD, lightweight tag: light-tag)
/// ```
///
/// plus `blob-tag`, an annotated tag whose target is the blob `a.txt`, to confirm
/// `verify-tag` accepts annotated tags of non-commit objects and echoes their
/// bodies verbatim under `-v`.
fn build_repo() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("verify-tag");
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

    let blob = git_capture(&repo, &["rev-parse", "HEAD:a.txt"]);
    git_at(
        &repo,
        &["tag", "-a", "-m", "blob tag", "blob-tag", blob.as_str()],
        "@1790000300 -0500",
    );

    (root, repo)
}

#[test]
fn verify_tag_unsigned_annotated_tags_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // An unsigned annotated tag has no signature: git prints `error: no signature
    // found` and exits 1, and under -v it first echoes the full tag body to stdout.
    let anno_oid = git_capture(&repo, &["rev-parse", "anno-tag"]);

    for args in [
        vec!["verify-tag", "anno-tag"],
        vec!["verify-tag", "-v", "anno-tag"],
        vec!["verify-tag", "--verbose", "anno-tag"],
        // --raw only affects signed tags; for an unsigned tag stdout stays empty.
        vec!["verify-tag", "--raw", "anno-tag"],
        vec!["verify-tag", "-v", "--raw", "anno-tag"],
        // Referenced by OID rather than name (verify-tag does not peel, so a tag
        // OID stays a tag).
        vec!["verify-tag", anno_oid.as_str()],
        vec!["verify-tag", "-v", anno_oid.as_str()],
        // An annotated tag of a blob: still a tag object, body echoed under -v.
        vec!["verify-tag", "blob-tag"],
        vec!["verify-tag", "-v", "blob-tag"],
        // Multiple unsigned tags: one error per tag, bodies (under -v) in order.
        vec!["verify-tag", "anno-tag", "blob-tag"],
        vec!["verify-tag", "-v", "anno-tag", "blob-tag"],
        // `--` terminates option parsing.
        vec!["verify-tag", "--", "anno-tag"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_tag_non_tag_objects_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    let commit = git_capture(&repo, &["rev-parse", "HEAD"]);
    let tree = git_capture(&repo, &["rev-parse", "HEAD^{tree}"]);
    let blob = git_capture(&repo, &["rev-parse", "HEAD:a.txt"]);

    for args in [
        // A commit OID/ref: cannot verify a non-tag object of type commit.
        vec!["verify-tag", "HEAD"],
        vec!["verify-tag", commit.as_str()],
        vec!["verify-tag", "-v", "HEAD"],
        // A lightweight tag points straight at the commit; verify-tag does not
        // peel, so it is reported as a non-tag object of type commit.
        vec!["verify-tag", "light-tag"],
        // A tree OID: type tree. The error echoes the argument verbatim.
        vec!["verify-tag", tree.as_str()],
        vec!["verify-tag", "HEAD^{tree}"],
        // A blob OID: type blob.
        vec!["verify-tag", blob.as_str()],
        vec!["verify-tag", "HEAD:a.txt"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_tag_unresolvable_arguments_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    for args in [
        vec!["verify-tag", "no-such-tag"],
        vec!["verify-tag", "1234"],
        vec!["verify-tag", "0000000000000000000000000000000000000000"],
        // All arguments are processed; two bad refs report two errors, exit 1.
        vec!["verify-tag", "nope-one", "nope-two"],
        // A bad ref followed by a good (unsigned) tag: bad reports "not found",
        // good reports "no signature found", overall exit 1.
        vec!["verify-tag", "nope", "anno-tag"],
        // A non-tag object followed by a bad ref: both report, in order.
        vec!["verify-tag", "HEAD", "nope"],
        // `--` makes a flag-looking token an (unresolvable) tag operand.
        vec!["verify-tag", "--", "-v"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_tag_usage_and_option_errors_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    for args in [
        // No tag operand: usage error, exit 129.
        vec!["verify-tag"],
        // Unknown long option: error + usage, exit 129.
        vec!["verify-tag", "--bogus"],
        vec!["verify-tag", "--bogus", "anno-tag"],
        // Unknown short switch: error + usage, exit 129.
        vec!["verify-tag", "-z"],
        vec!["verify-tag", "-z", "anno-tag"],
        // Value mishandling: a one-line parse-options error, *no* usage, exit 129.
        vec!["verify-tag", "--verbose=1"],
        vec!["verify-tag", "--raw=1"],
        vec!["verify-tag", "--format"],
        // `-h` prints the short usage block to stdout, exit 129.
        vec!["verify-tag", "-h"],
        // NB: `--help` is intentionally *not* exercised here. Real git treats
        // `git verify-tag --help` as a request for the manual page and execs
        // `man git-verify-tag`, whose output embeds the locally-installed git
        // version and the page's build date (e.g. "Git 2.54.0 ... 2026-04-19")
        // plus `man`-specific overstrike/formatting that varies with the host's
        // terminal, locale, and man implementation. That output is inherently
        // non-reproducible and host-specific, so a byte-for-byte differential
        // assertion against it cannot pass portably; `-h` above is the stable,
        // self-emitted usage path and is the meaningful thing to pin.
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}
