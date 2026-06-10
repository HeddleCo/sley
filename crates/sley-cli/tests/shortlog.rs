//! Differential interop tests for `git shortlog` against the system `git` binary.
//!
//! A temp repository is built with real `git` using a fixed committer identity and
//! fixed dates, but with per-commit *author* identities so the grouping/sorting
//! logic has something to chew on. The same repository is then summarised by both
//! `git` and `sley` and the stdout/stderr/exit-code are required to match
//! byte-for-byte. The whole suite is skipped when `git --version` is unavailable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Fixed identity/date environment shared by every invocation. Author identity is
/// overridden per commit via [`commit_as`]; everything else stays constant so the
/// fixtures are deterministic.
fn base_command(program: &str, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        // Keep config hermetic so a developer's ~/.gitconfig cannot perturb output.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    command
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = base_command("git", cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a commit authored by `name <email>` (committer stays the fixed Tester
/// identity) so author-grouping differs from committer-grouping.
fn commit_as(cwd: &Path, name: &str, email: &str, file: &str, content: &str, message: &str) {
    fs::write(cwd.join(file), content).expect("write fixture");
    git_ok(cwd, &["add", file]);
    let output = base_command("git", cwd)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .args(["commit", "-q", "-m", message])
        .output()
        .unwrap_or_else(|err| panic!("failed to commit as {name}: {err}"));
    assert!(
        output.status.success(),
        "commit as {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a commit whose committer (not just author) is `name <email>`, used to
/// exercise `--committer`/`--group=committer`.
fn commit_as_committer(
    cwd: &Path,
    name: &str,
    email: &str,
    file: &str,
    content: &str,
    message: &str,
) {
    fs::write(cwd.join(file), content).expect("write fixture");
    git_ok(cwd, &["add", file]);
    let output = base_command("git", cwd)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .args(["commit", "-q", "-m", message])
        .output()
        .unwrap_or_else(|err| panic!("failed to commit as committer {name}: {err}"));
    assert!(
        output.status.success(),
        "commit as committer {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    base_command(program, cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = base_command(program, cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin is piped"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn git_rs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn assert_same(repo: &Path, args: &[&str]) {
    let mut git_args = vec!["shortlog"];
    git_args.extend_from_slice(args);
    let mut rs_args = vec!["shortlog"];
    rs_args.extend_from_slice(args);
    let expected = run("git", repo, &git_args);
    let actual = run(git_rs_bin(), repo, &rs_args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit code differed for shortlog {args:?}\ngit stderr:\n{}\nsley stderr:\n{}",
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for shortlog {args:?}",
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for shortlog {args:?}",
    );
}

fn assert_same_stdin(repo: &Path, args: &[&str], stdin: &[u8]) {
    let mut git_args = vec!["shortlog"];
    git_args.extend_from_slice(args);
    let mut rs_args = vec!["shortlog"];
    rs_args.extend_from_slice(args);
    let expected = run_with_stdin("git", repo, &git_args, stdin);
    let actual = run_with_stdin(git_rs_bin(), repo, &rs_args, stdin);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit code differed for shortlog (stdin) {args:?}",
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for shortlog (stdin) {args:?}",
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for shortlog (stdin) {args:?}",
    );
}

/// Build a repository with a spread of authors, subjects, and a multi-author
/// committer story for grouping tests.
fn build_repo(name: &str) -> PathBuf {
    let repo = unique_temp_dir(name);
    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.name", "Tester"]);
    git_ok(&repo, &["config", "user.email", "tester@example.com"]);

    // Alice: two commits, one with a multi-line message body and a folded subject.
    commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        "f",
        "a",
        "first commit",
    );
    commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        "f",
        "ab",
        "second commit\n\nwith a body that must be ignored",
    );
    // Bob: a commit whose subject has leading/trailing whitespace to trim, and one
    // whose subject is wrapped across two physical lines (folded with a space).
    commit_as(
        &repo,
        "Bob",
        "bob@example.com",
        "f",
        "abc",
        "   bob spaced subject   ",
    );
    commit_as(
        &repo,
        "Bob",
        "bob@example.com",
        "f",
        "abcd",
        "bob wrapped subject line one\nand a continuation line still in the subject",
    );
    // Alice under a different email: groups with Alice without -e, splits with -e.
    commit_as(
        &repo,
        "Alice",
        "alice-alt@example.com",
        "f",
        "abcde",
        "alice alt email",
    );
    // A lowercase author to exercise case-sensitive ASCII sorting.
    commit_as(
        &repo,
        "zoe",
        "zoe@example.com",
        "f",
        "abcdef",
        "zoe lowercase author",
    );
    repo
}

#[test]
fn shortlog_default_and_flag_matrix_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("shortlog-matrix");
    let cases: &[&[&str]] = &[
        &["HEAD"],
        &["-n", "HEAD"],
        &["--numbered", "HEAD"],
        &["-s", "HEAD"],
        &["--summary", "HEAD"],
        &["-sn", "HEAD"],
        &["-ns", "HEAD"],
        &["-e", "HEAD"],
        &["--email", "HEAD"],
        &["-se", "HEAD"],
        &["-sne", "HEAD"],
        &["-sen", "HEAD"],
        &["-nse", "HEAD"],
        &["-s", "--no-summary", "HEAD"],
        &["--no-summary", "-s", "HEAD"],
        &["-n", "--no-numbered", "HEAD"],
        &["-e", "--no-email", "HEAD"],
    ];
    for case in cases {
        assert_same(&repo, case);
    }
}

#[test]
fn shortlog_committer_grouping_matches_git() {
    if !git_available() {
        return;
    }
    let repo = unique_temp_dir("shortlog-committer");
    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.name", "Tester"]);
    git_ok(&repo, &["config", "user.email", "tester@example.com"]);
    // Authors differ from committers so author vs committer grouping diverges.
    commit_as_committer(&repo, "Carol", "carol@example.com", "f", "1", "carol one");
    commit_as_committer(&repo, "Carol", "carol@example.com", "f", "12", "carol two");
    commit_as_committer(&repo, "Dave", "dave@example.com", "f", "123", "dave one");

    let cases: &[&[&str]] = &[
        &["-s", "HEAD"],
        &["-sc", "HEAD"],
        &["-s", "--committer", "HEAD"],
        &["-se", "--committer", "HEAD"],
        &["-s", "--group=committer", "HEAD"],
        &["-s", "--group=author", "HEAD"],
        &["-sc", "--no-committer", "HEAD"],
    ];
    for case in cases {
        assert_same(&repo, case);
    }
}

#[test]
fn shortlog_max_count_and_range_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("shortlog-range");
    let cases: &[&[&str]] = &[
        &["-1", "HEAD"],
        &["-2", "HEAD"],
        &["-3", "HEAD"],
        &["-s", "-2", "HEAD"],
        &["-s", "--max-count=2", "HEAD"],
        &["--max-count=3", "HEAD"],
        &["-s", "HEAD~3..HEAD"],
        &["HEAD~2..HEAD"],
        &["-s", "HEAD~5..HEAD~2"],
    ];
    for case in cases {
        assert_same(&repo, case);
    }
}

#[test]
fn shortlog_wrap_matches_git() {
    if !git_available() {
        return;
    }
    let repo = unique_temp_dir("shortlog-wrap");
    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.name", "Tester"]);
    git_ok(&repo, &["config", "user.email", "tester@example.com"]);
    commit_as(
        &repo,
        "Wrapper",
        "wrap@example.com",
        "f",
        "1",
        "a fairly long subject line that definitely exceeds the wrap width so we can verify the linewrap output indentation matches upstream behaviour exactly",
    );
    commit_as(
        &repo,
        "Wrapper",
        "wrap@example.com",
        "f",
        "12",
        "short subject",
    );
    let cases: &[&[&str]] = &[
        &["-w", "HEAD"],
        &["-w50", "HEAD"],
        &["-w50,4", "HEAD"],
        &["-w50,4,8", "HEAD"],
        &["-w0", "HEAD"],
        &["-w20,0,0", "HEAD"],
    ];
    for case in cases {
        assert_same(&repo, case);
    }
}

#[test]
fn shortlog_author_and_grep_filter_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("shortlog-filter");
    let cases: &[&[&str]] = &[
        &["-s", "--author=Alice", "HEAD"],
        &["-s", "--author=Bob", "HEAD"],
        &["-s", "--grep=wrapped", "HEAD"],
        &["-s", "--author=alice", "-i", "HEAD"],
    ];
    for case in cases {
        assert_same(&repo, case);
    }
}

#[test]
fn shortlog_stdin_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("shortlog-stdin");
    // Feed the canonical `git log --pretty=short` stream into both binaries.
    let log = run("git", &repo, &["log", "--pretty=short", "HEAD"]).stdout;
    let cases: &[&[&str]] = &[&[], &["-s"], &["-e"], &["-se"], &["-sn"], &["--committer"]];
    for case in cases {
        assert_same_stdin(&repo, case, &log);
    }
    // Also exercise the `--format=medium` (default) stream, which includes a Date:
    // header line that must be skipped.
    let medium = run("git", &repo, &["log", "HEAD"]).stdout;
    assert_same_stdin(&repo, &["-s"], &medium);
    // The `--format=fuller` stream carries `Commit:` headers, so committer grouping
    // from stdin produces real output rather than an empty result.
    let fuller = run("git", &repo, &["log", "--format=fuller", "HEAD"]).stdout;
    assert_same_stdin(&repo, &["-s", "--committer"], &fuller);
    assert_same_stdin(&repo, &["-se", "--committer"], &fuller);
    assert_same_stdin(&repo, &["-s"], &fuller);
}

#[test]
fn shortlog_help_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("shortlog-help");
    // `-h` prints usage to stdout and exits 129 in upstream git.
    assert_same(&repo, &["-h"]);
}
