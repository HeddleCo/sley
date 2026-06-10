//! Differential interop tests for `git bisect` vs the system git binary.
//!
//! `git bisect` is a stateful command: each subcommand reads and rewrites the
//! bisection state under `.git`, and the bisection step checks out a computed
//! midpoint. To compare against the real `git` binary we build two byte-for-byte
//! identical repositories (same fixed identity/date environment, so commit
//! object ids are reproducible) and drive the *same* sequence of bisect
//! subcommands through `git` in one and `sley` in the other, asserting that
//! stdout and the exit code agree at every step.
//!
//! The topologies are chosen so the midpoint is unambiguous (linear histories
//! whose candidate count is even, which gives a unique optimal bisection point),
//! making the comparison deterministic across both implementations. The whole
//! file is gated on `git --version` succeeding, so it is a no-op where git is
//! absent.

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

/// The fixed identity/date environment the task pins, applied to every command
/// so that commit object ids are reproducible across `git` and `sley`.
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

/// Run a repository-building `git` command at a specific author/committer date so
/// each commit gets a distinct, deterministic timestamp (and therefore a stable
/// object id). Aborts on failure.
fn git_at(cwd: &Path, args: &[&str], date: &str) {
    let output = Command::new(sley_testkit::oracle_git())
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

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
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

/// A pair of identical repositories: one driven by `git`, one by `sley`.
struct RepoPair {
    git_repo: PathBuf,
    rs_repo: PathBuf,
}

impl RepoPair {
    /// Build two identical linear repositories, each with `count` commits named
    /// `c1`..`c<count>`, each appending a line to `f.txt` at a distinct date.
    fn linear(root: &Path, count: usize) -> Self {
        let git_repo = root.join("git_repo");
        let rs_repo = root.join("rs_repo");
        for repo in [&git_repo, &rs_repo] {
            fs::create_dir_all(repo).expect("create repo dir");
            git_at(repo, &["init", "-q", "-b", "main"], "@1790000000 -0500");
            for i in 1..=count {
                let path = repo.join("f.txt");
                let mut contents = fs::read_to_string(&path).unwrap_or_default();
                contents.push_str(&format!("line {i}\n"));
                fs::write(&path, contents).expect("write f.txt");
                git_at(repo, &["add", "f.txt"], "@1790000000 -0500");
                let date = format!("@{} -0500", 1_790_000_000 + i as i64);
                git_at(repo, &["commit", "-q", "-m", &format!("c{i}")], &date);
            }
        }
        Self { git_repo, rs_repo }
    }

    /// The full object id of the first (root) commit, shared by both repos.
    fn first_oid(&self) -> String {
        let out = git(&self.git_repo, &["rev-list", "--max-parents=0", "HEAD"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The full object id of the tip commit, shared by both repos.
    fn last_oid(&self) -> String {
        let out = git(&self.git_repo, &["rev-parse", "HEAD"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Run the same args through both binaries and assert stdout + exit code
    /// agree. Returns the shared stdout for further inspection.
    fn assert_same(&self, args: &[&str]) -> String {
        let g = git(&self.git_repo, args);
        let r = git_rs(&self.rs_repo, args);
        let g_out = String::from_utf8_lossy(&g.stdout).into_owned();
        let r_out = String::from_utf8_lossy(&r.stdout).into_owned();
        assert_eq!(
            r_out,
            g_out,
            "stdout differs for `bisect {args:?}`\n  sley stderr: {}\n  git stderr: {}",
            String::from_utf8_lossy(&r.stderr),
            String::from_utf8_lossy(&g.stderr),
        );
        assert_eq!(
            r.status.code(),
            g.status.code(),
            "exit code differs for `bisect {args:?}`\n  git stdout: {g_out}\n  sley stderr: {}",
            String::from_utf8_lossy(&r.stderr),
        );
        g_out
    }

    /// Assert that the current HEAD commit id matches between the two repos.
    fn assert_head_matches(&self) {
        let g = git(&self.git_repo, &["rev-parse", "HEAD"]);
        let r = git(&self.rs_repo, &["rev-parse", "HEAD"]);
        assert_eq!(
            String::from_utf8_lossy(&r.stdout),
            String::from_utf8_lossy(&g.stdout),
            "detached HEAD after bisect step differs",
        );
    }

    /// Read a bisect state file (relative to `.git`) from a repo, returning an
    /// empty string when it is absent.
    fn state(repo: &Path, name: &str) -> String {
        fs::read_to_string(repo.join(".git").join(name)).unwrap_or_default()
    }

    /// Assert a `.git/<name>` state file is byte-identical between repos.
    fn assert_state_matches(&self, name: &str) {
        assert_eq!(
            Self::state(&self.rs_repo, name),
            Self::state(&self.git_repo, name),
            "state file `.git/{name}` differs after bisect step",
        );
    }
}

/// `git bisect start <bad> <good>` on a linear history must print the same
/// "Bisecting:" banner and midpoint, detach HEAD to the same commit, and write
/// identical BISECT_* state and refs.
#[test]
fn bisect_start_with_revs_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-start");
    // 7 commits -> 6 candidates (even) -> unique midpoint.
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    let out = pair.assert_same(&["bisect", "start", &bad, &good]);
    assert!(
        out.starts_with("Bisecting: 2 revisions left to test after this (roughly 2 steps)\n"),
        "unexpected start banner: {out:?}",
    );
    pair.assert_head_matches();
    pair.assert_state_matches("BISECT_LOG");
    pair.assert_state_matches("BISECT_TERMS");
    pair.assert_state_matches("BISECT_NAMES");
    pair.assert_state_matches("BISECT_START");
    pair.assert_state_matches("BISECT_EXPECTED_REV");
    // The known-bad ref should hold the bad commit in both repos.
    assert_eq!(
        RepoPair::state(&pair.rs_repo, "refs/bisect/bad").trim(),
        bad,
    );
    assert_eq!(
        RepoPair::state(&pair.git_repo, "refs/bisect/bad").trim(),
        bad,
    );

    fs::remove_dir_all(&root).ok();
}

/// A full bisection run (start, then alternating good/bad) must agree at every
/// step, including the final "<oid> is the first bad commit" announcement and
/// the `git show`-style commit summary that follows it.
#[test]
fn bisect_full_run_converges_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-run");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    pair.assert_same(&["bisect", "start", &bad, &good]);
    pair.assert_head_matches();
    // Walk the search to convergence with a fixed answer pattern; each step
    // reads the same detached HEAD in both repos, so "good"/"bad" with no rev
    // mark the same commit.
    pair.assert_same(&["bisect", "good"]);
    pair.assert_head_matches();
    pair.assert_same(&["bisect", "bad"]);
    pair.assert_head_matches();
    let final_out = pair.assert_same(&["bisect", "good"]);
    assert!(
        final_out.contains("is the first bad commit"),
        "expected convergence message, got: {final_out:?}",
    );
    pair.assert_state_matches("BISECT_LOG");

    fs::remove_dir_all(&root).ok();
}

/// `git bisect log` must reproduce the recorded transcript, and the status
/// lines emitted while waiting for commits must match git exactly.
#[test]
fn bisect_log_and_status_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-log");
    let pair = RepoPair::linear(&root, 7);
    let good = pair.first_oid();

    // Starting with no revs leaves the bisection waiting for both endpoints.
    let waiting = pair.assert_same(&["bisect", "start"]);
    assert_eq!(waiting, "status: waiting for both good and bad commits\n");
    // Marking the tip bad leaves it waiting for a good commit.
    let waiting_good = pair.assert_same(&["bisect", "bad"]);
    assert_eq!(
        waiting_good,
        "status: waiting for good commit(s), bad commit known\n",
    );
    // Marking a good commit kicks off the first real step.
    pair.assert_same(&["bisect", "good", &good]);
    pair.assert_head_matches();
    // The recorded log must be byte-identical.
    pair.assert_same(&["bisect", "log"]);
    pair.assert_state_matches("BISECT_LOG");

    fs::remove_dir_all(&root).ok();
}

/// Custom terms via `--term-old`/`--term-new` must be honoured throughout:
/// the alternative subcommands mark commits, `bisect terms` reports the pair,
/// and BISECT_TERMS/BISECT_LOG match git.
#[test]
fn bisect_custom_terms_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-terms");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    pair.assert_same(&[
        "bisect",
        "start",
        "--term-old",
        "fast",
        "--term-new",
        "slow",
    ]);
    pair.assert_state_matches("BISECT_TERMS");
    pair.assert_same(&["bisect", "slow", &bad]);
    pair.assert_same(&["bisect", "fast", &good]);
    pair.assert_head_matches();
    pair.assert_same(&["bisect", "terms"]);
    pair.assert_same(&["bisect", "terms", "--term-good"]);
    pair.assert_same(&["bisect", "terms", "--term-bad"]);
    pair.assert_state_matches("BISECT_LOG");

    fs::remove_dir_all(&root).ok();
}

/// `git bisect skip` must keep the announced count tied to the optimal split
/// while checking out a non-skipped neighbour, matching git's choice.
#[test]
fn bisect_skip_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-skip");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    pair.assert_same(&["bisect", "start", &bad, &good]);
    pair.assert_head_matches();
    // Skip the midpoint: git re-bisects to a neighbouring commit but keeps the
    // same "revisions left" banner.
    pair.assert_same(&["bisect", "skip"]);
    pair.assert_head_matches();
    pair.assert_state_matches("BISECT_LOG");

    fs::remove_dir_all(&root).ok();
}

/// `git bisect reset` must clear all bisection state and restore HEAD to the
/// starting branch, with matching (empty) stdout and exit code.
#[test]
fn bisect_reset_restores_branch_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-reset");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    pair.assert_same(&["bisect", "start", &bad, &good]);
    pair.assert_same(&["bisect", "reset"]);

    // HEAD is back on the branch and the bisection state/refs are gone in both.
    // (git leaves an empty `refs/bisect` directory behind, so we check that the
    // bisection ref *files* are gone rather than the directory itself.)
    for repo in [&pair.git_repo, &pair.rs_repo] {
        let head = RepoPair::state(repo, "HEAD");
        assert_eq!(head.trim(), "ref: refs/heads/main", "HEAD not restored");
        assert!(
            !repo.join(".git/BISECT_START").exists(),
            "BISECT_START not removed",
        );
        let bisect_refs = repo.join(".git/refs/bisect");
        let remaining = fs::read_dir(&bisect_refs)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "refs/bisect/* not removed");
    }

    fs::remove_dir_all(&root).ok();
}

/// Error and usage paths must match git: marking before `start`, `log` when not
/// bisecting, an unknown subcommand, and a missing subcommand.
#[test]
fn bisect_error_paths_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-errors");
    let pair = RepoPair::linear(&root, 5);

    // Marking before starting: identical message + exit 1.
    pair.assert_same(&["bisect", "bad"]);
    pair.assert_same(&["bisect", "good"]);
    // `log` when not bisecting: identical message + exit 1.
    pair.assert_same(&["bisect", "log"]);
    // `reset` when not bisecting: no-op, exit 0.
    pair.assert_same(&["bisect", "reset"]);
    // Unknown subcommand: usage + exit 129.
    pair.assert_same(&["bisect", "frobnicate"]);
    // No subcommand: usage + exit 129.
    pair.assert_same(&["bisect"]);

    fs::remove_dir_all(&root).ok();
}

/// Inconsistent endpoints must be rejected the same way: a commit marked both
/// good and bad, and a good commit that is a descendant of the bad commit.
#[test]
fn bisect_inconsistent_endpoints_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-bad-endpoints");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    // Same commit marked good and bad.
    pair.assert_same(&["bisect", "start"]);
    pair.assert_same(&["bisect", "bad", &bad]);
    pair.assert_same(&["bisect", "good", &bad]);
    pair.assert_same(&["bisect", "reset"]);

    // A "good" commit that is actually newer than the "bad" commit.
    pair.assert_same(&["bisect", "start"]);
    pair.assert_same(&["bisect", "bad", &good]);
    pair.assert_same(&["bisect", "good", &bad]);
    pair.assert_same(&["bisect", "reset"]);

    fs::remove_dir_all(&root).ok();
}

/// Argument-validation diagnostics during a bisection must match git: an
/// unresolvable rev passed to `good`, and more than one rev passed to `bad`.
#[test]
fn bisect_argument_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-arg-errors");
    let pair = RepoPair::linear(&root, 7);
    let a = pair.last_oid();
    let b = {
        let out = git(&pair.git_repo, &["rev-parse", "HEAD~1"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    pair.assert_same(&["bisect", "start"]);
    // `good` with a rev that does not resolve.
    pair.assert_same(&["bisect", "good", "nonexistent-rev"]);
    // `bad` with two revs (it accepts only one).
    pair.assert_same(&["bisect", "bad", &a, &b]);
    pair.assert_same(&["bisect", "reset"]);

    fs::remove_dir_all(&root).ok();
}

/// `git bisect start --no-checkout` keeps the working tree on its branch and
/// tracks the candidate commit in BISECT_HEAD instead of detaching HEAD. The
/// announced banner, BISECT_HEAD, and BISECT_LOG must all match git, and `good`
/// with no rev must mark the BISECT_HEAD commit.
#[test]
fn bisect_no_checkout_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("bisect-no-checkout");
    let pair = RepoPair::linear(&root, 7);
    let bad = pair.last_oid();
    let good = pair.first_oid();

    pair.assert_same(&["bisect", "start", "--no-checkout", &bad, &good]);
    // HEAD must NOT have detached; the candidate lives in BISECT_HEAD.
    for repo in [&pair.git_repo, &pair.rs_repo] {
        assert_eq!(
            RepoPair::state(repo, "HEAD").trim(),
            "ref: refs/heads/main",
            "--no-checkout must not detach HEAD",
        );
    }
    pair.assert_state_matches("BISECT_HEAD");
    // Marking the BISECT_HEAD commit good (no rev) advances the search.
    pair.assert_same(&["bisect", "good"]);
    pair.assert_state_matches("BISECT_HEAD");
    pair.assert_state_matches("BISECT_LOG");

    fs::remove_dir_all(&root).ok();
}
