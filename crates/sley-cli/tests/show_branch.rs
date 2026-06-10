//! Differential interop tests for `git show-branch` vs the system `git` binary.
//!
//! Each test builds a throwaway repository with the system `git` and asserts
//! that `sley show-branch ...` produces byte-identical stdout, stderr, and
//! exit code to `git show-branch ...`. The whole suite is gated on
//! `git --version` succeeding, so it is a no-op where git is unavailable.
//!
//! Identity is fixed to the values the task mandates
//! (`GIT_AUTHOR_NAME`/`GIT_COMMITTER_NAME` = `Tester`, the matching emails, and
//! `GIT_AUTHOR_DATE` = `GIT_COMMITTER_DATE` = `@1790000000 -0500`). show-branch
//! orders the matrix body by commit date, and its commit-naming walk is
//! date-driven; to keep that ordering unambiguous (and therefore identical
//! between the two binaries) the fixtures advance the committer timestamp by a
//! fixed step per commit, starting from that base. Object ids and the printed
//! `[name] subject` lines are then fully deterministic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// Base committer timestamp (the task-mandated value); fixtures step forward
/// from here so each commit has a distinct, increasing date.
const BASE_EPOCH: i64 = 1_790_000_000;
const STEP: i64 = 100;

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// Run a program with the fixed identity the task mandates and a specific
/// author/committer date (in `@<seconds> <tz>` form), so object ids and dates
/// are reproducible across both binaries.
fn run_env_dated(program: &str, cwd: &Path, args: &[&str], date: &str) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        // Force non-color, non-paged output regardless of the caller's config so
        // the comparison is stable.
        .env("GIT_PAGER", "cat")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

/// The base date string the task mandates; used for non-committing git calls.
fn base_date() -> String {
    format!("@{BASE_EPOCH} -0500")
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env_dated(sley_testkit::oracle_git(), cwd, args, &base_date())
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_env_dated(env!("CARGO_BIN_EXE_sley"), cwd, args, &base_date())
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A small commit driver that advances the committer date per commit, so the
/// matrix body order and commit names are deterministic.
struct RepoBuilder {
    repo: PathBuf,
    step: i64,
}

impl RepoBuilder {
    fn new(name: &str, default_branch: &str) -> Self {
        let root = unique_temp_dir(name);
        let repo = root.join("repo");
        git_ok(
            &root,
            &[
                "init",
                "-q",
                "-b",
                default_branch,
                repo.to_str().expect("test operation should succeed"),
            ],
        );
        Self { repo, step: 0 }
    }

    fn date(&self) -> String {
        format!("@{} -0500", BASE_EPOCH + self.step * STEP)
    }

    /// Write `content` to `file`, stage it, and commit with `message`.
    fn commit_file(&mut self, file: &str, content: &str, message: &str) {
        fs::write(self.repo.join(file), content).expect("write file");
        git_ok(&self.repo, &["add", file]);
        let out = run_env_dated(sley_testkit::oracle_git(), &self.repo, &["commit", "-qm", message], &self.date());
        assert!(
            out.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.step += 1;
    }

    /// `--no-ff` merge of `branch` into the current branch.
    fn merge_no_ff(&mut self, branch: &str, message: &str) {
        let out = run_env_dated(
            sley_testkit::oracle_git(),
            &self.repo,
            &["merge", "-q", "--no-ff", "--no-edit", "-m", message, branch],
            &self.date(),
        );
        assert!(
            out.status.success(),
            "merge failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.step += 1;
    }

    fn checkout_new(&mut self, branch: &str, start: &str) {
        git_ok(&self.repo, &["checkout", "-q", "-b", branch, start]);
    }

    fn checkout(&mut self, branch: &str) {
        git_ok(&self.repo, &["checkout", "-q", branch]);
    }

    fn run(&self, args: &[&str]) {
        git_ok(&self.repo, args);
    }
}

/// Assert `sley show-branch <args>` matches `git show-branch <args>` on
/// stdout, stderr, and exit code.
fn assert_same(repo: &Path, args: &[&str]) {
    let g = git(repo, args);
    let r = git_rs(repo, args);
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
        "exit differs for {args:?}"
    );
}

/// Build a repo with two branches that diverge after a shared base, then a
/// commit on each side so there is a non-trivial merge base.
///
/// ```text
///   main:   A - B - M1
///                 \
///   topic:         T1 - T2
/// ```
fn build_diverged(name: &str) -> RepoBuilder {
    let mut b = RepoBuilder::new(name, "main");
    b.commit_file("a", "1\n", "A");
    b.commit_file("a", "2\n", "B");
    b.checkout_new("topic", "main");
    b.commit_file("t", "t1\n", "T1");
    b.commit_file("t", "t2\n", "T2");
    b.checkout("main");
    b.commit_file("m", "m1\n", "M1");
    b
}

/// Build a repo with a merge commit so the `-` merge marker and merge-aware
/// naming (`name^2`) are exercised.
///
/// ```text
///   main:  A - C - M(erge) - D - E
///           \     /
///   topic:   B1 - B2   (merged into main at M)
/// ```
fn build_merged(name: &str) -> RepoBuilder {
    let mut b = RepoBuilder::new(name, "main");
    b.commit_file("a", "a\n", "A");
    b.checkout_new("topic", "main");
    b.commit_file("b", "b1\n", "B1");
    b.commit_file("b", "b2\n", "B2");
    b.checkout("main");
    b.commit_file("c", "c\n", "C");
    b.merge_no_ff("topic", "Merge topic");
    b.commit_file("d", "d\n", "D");
    b.commit_file("e", "e\n", "E");
    b
}

/// Build a three-branch fan-out so the matrix has three columns and the header
/// uses the `!`/`*` markers, plus a lightweight and annotated tag.
fn build_three(name: &str) -> RepoBuilder {
    let mut b = RepoBuilder::new(name, "main");
    b.commit_file("base", "base\n", "base");
    b.checkout_new("feature-a", "main");
    b.commit_file("a", "a1\n", "a1");
    b.commit_file("a", "a2\n", "a2");
    b.checkout_new("feature-b", "main");
    b.commit_file("bb", "b1\n", "b1");
    b.checkout("main");
    b.commit_file("m", "m1\n", "m1");
    b.run(&["tag", "v1"]);
    b
}

#[test]
fn diverged_default_and_list_match_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-diverged");
    let repo = &b.repo;

    // Default selection (all local branches), the matrix, and the list view.
    assert_same(repo, &["show-branch"]);
    assert_same(repo, &["show-branch", "--all"]);
    assert_same(repo, &["show-branch", "--list"]);
    assert_same(repo, &["show-branch", "--list", "--all"]);
    assert_same(repo, &["show-branch", "main", "topic"]);
    assert_same(repo, &["show-branch", "topic", "main"]);
    // A single rev prints just `[name] subject` with no matrix.
    assert_same(repo, &["show-branch", "topic"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn diverged_more_and_ordering_match_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-diverged-more");
    let repo = &b.repo;

    for args in [
        vec!["show-branch", "--more=0", "main", "topic"],
        vec!["show-branch", "--more=1", "main", "topic"],
        vec!["show-branch", "--more=5", "main", "topic"],
        vec!["show-branch", "--more=20", "main", "topic"],
        vec!["show-branch", "--topo-order", "--more=20", "main", "topic"],
        vec!["show-branch", "--date-order", "--more=20", "main", "topic"],
        vec!["show-branch", "--no-name", "--more=20", "main", "topic"],
        vec!["show-branch", "--sha1-name", "--more=20", "main", "topic"],
    ] {
        assert_same(repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn diverged_merge_base_and_independent_match_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-diverged-mb");
    let repo = &b.repo;

    assert_same(repo, &["show-branch", "--merge-base", "main", "topic"]);
    assert_same(repo, &["show-branch", "--merge-base", "--all"]);
    assert_same(repo, &["show-branch", "--independent", "main", "topic"]);
    assert_same(repo, &["show-branch", "--independent", "--all"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn merge_history_matrix_matches_git() {
    if !git_available() {
        return;
    }
    let b = build_merged("sb-merged");
    let repo = &b.repo;

    for args in [
        vec!["show-branch"],
        vec!["show-branch", "--more=20"],
        vec!["show-branch", "--more=20", "--all"],
        // `--sparse` keeps merges the dense default omits.
        vec!["show-branch", "--sparse", "--more=20", "--all"],
        vec!["show-branch", "--topo-order", "--more=20", "--all"],
        vec!["show-branch", "--date-order", "--more=20", "--all"],
        vec!["show-branch", "main", "topic"],
        vec!["show-branch", "--more=20", "main", "topic"],
        vec!["show-branch", "--merge-base", "main", "topic"],
        vec!["show-branch", "--independent", "--all"],
        vec!["show-branch", "--sha1-name", "--more=20", "main", "topic"],
        vec!["show-branch", "--no-name", "--more=20", "main", "topic"],
    ] {
        assert_same(repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn three_branch_matrix_matches_git() {
    if !git_available() {
        return;
    }
    let b = build_three("sb-three");
    let repo = &b.repo;

    for args in [
        vec!["show-branch"],
        vec!["show-branch", "--all"],
        vec!["show-branch", "main", "feature-a", "feature-b"],
        vec!["show-branch", "feature-b", "feature-a", "main"],
        vec!["show-branch", "--more=20", "main", "feature-a", "feature-b"],
        vec![
            "show-branch",
            "--topics",
            "--more=20",
            "main",
            "feature-a",
            "feature-b",
        ],
        vec![
            "show-branch",
            "--merge-base",
            "main",
            "feature-a",
            "feature-b",
        ],
        vec![
            "show-branch",
            "--independent",
            "main",
            "feature-a",
            "feature-b",
        ],
        // A tag as a rev argument, and a tag mixed with a branch.
        vec!["show-branch", "v1", "feature-a"],
        vec!["show-branch", "--more=20", "v1", "feature-a"],
        // A glob selecting the feature branches.
        vec!["show-branch", "feature-*"],
        vec!["show-branch", "--more=20", "feature-*"],
        // `--current` includes HEAD's branch when it is not named.
        vec!["show-branch", "--current", "feature-a"],
    ] {
        assert_same(repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn revision_expressions_match_git() {
    if !git_available() {
        return;
    }
    let b = build_merged("sb-revexpr");
    let repo = &b.repo;

    // Revision expressions are valid rev arguments and keep their literal name.
    assert_same(repo, &["show-branch", "main", "HEAD~1"]);
    assert_same(repo, &["show-branch", "--more=20", "main", "HEAD~2"]);
    assert_same(repo, &["show-branch", "HEAD"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn remote_tracking_refs_match_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-remotes");
    let repo = &b.repo;
    // Fabricate a remote-tracking ref pointing at main's tip.
    b.run(&["update-ref", "refs/remotes/origin/main", "main"]);

    assert_same(repo, &["show-branch", "-r"]);
    assert_same(repo, &["show-branch", "--remotes"]);
    assert_same(repo, &["show-branch", "-a"]);
    assert_same(repo, &["show-branch", "--all"]);
    assert_same(repo, &["show-branch", "--all", "--list"]);
    assert_same(repo, &["show-branch", "--all", "--more=20"]);
    assert_same(repo, &["show-branch", "origin/main", "main"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn detached_head_current_matches_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-detached");
    let repo = &b.repo;
    b.run(&["checkout", "-q", "--detach", "HEAD"]);

    // In detached state `--current` appends the literal `HEAD` rev.
    assert_same(repo, &["show-branch", "--current", "main", "topic"]);
    assert_same(repo, &["show-branch", "--current"]);
    assert_same(repo, &["show-branch", "--current", "--all"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn errors_and_usage_match_git() {
    if !git_available() {
        return;
    }
    let b = build_diverged("sb-errors");
    let repo = &b.repo;

    // A nonexistent ref is fatal with git's exact message and exit 128.
    assert_same(repo, &["show-branch", "nonexistent"]);
    assert_same(repo, &["show-branch", "main", "nonexistent"]);
    // A glob that matches nothing warns and prints "No revs to be shown.".
    assert_same(repo, &["show-branch", "zzz-no-such-*"]);
    // `-h` and an unknown option both emit the usage block and exit 129.
    assert_same(repo, &["show-branch", "-h"]);
    assert_same(repo, &["show-branch", "--no-such-option"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn empty_repository_matches_git() {
    if !git_available() {
        return;
    }
    // A freshly-initialised repo with no commits: "No revs to be shown.".
    let b = RepoBuilder::new("sb-empty", "main");
    let repo = &b.repo;

    assert_same(repo, &["show-branch"]);
    assert_same(repo, &["show-branch", "--all"]);
    assert_same(repo, &["show-branch", "--list"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}
