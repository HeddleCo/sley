//! Differential interop tests for `git describe` vs the system git binary.
//!
//! Each test builds a temp repository with the real `git` binary, then runs the
//! same `describe` invocation through both `git` and `git-rs` and asserts that
//! stdout, stderr, and the exit code match. Because both binaries see the same
//! objects built under a fixed identity/date environment, abbreviated commit
//! object names are identical and can be compared byte-for-byte. The whole file
//! is gated on `git --version` succeeding, so it is a no-op where git is absent.

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

/// The fixed identity/date environment the task pins, applied to every command
/// (both `git` and `git-rs`) so that commit object ids are reproducible.
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
/// commits and annotated tags get distinct, deterministic timestamps. Aborts on
/// failure.
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
        "exit differs for {args:?}"
    );
}

fn write_commit(repo: &Path, contents: &str, message: &str, date: &str) {
    write_named_commit(repo, "f.txt", contents, message, date);
}

/// Like `write_commit` but writes a named file, so divergent branches can touch
/// disjoint paths and merge without conflicts.
fn write_named_commit(repo: &Path, file: &str, contents: &str, message: &str, date: &str) {
    fs::write(repo.join(file), contents).unwrap_or_else(|err| panic!("write {file}: {err}"));
    git_ok(repo, &["add", file]);
    git_at(repo, &["commit", "-q", "-m", message], date);
}

/// A linear history with both annotated and lightweight tags at varying depths:
///
/// ```text
/// c1 (annotated v1.0) - c2 (lightweight light-2) - c3 (annotated v2.0) - c4 - c5 (HEAD)
/// ```
fn build_linear_repo() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("describe-linear");
    let repo = root.join("repo");
    git_ok(
        &root,
        &["init", "-q", "-b", "main", repo.to_str().expect("utf8")],
    );

    write_commit(&repo, "a\n", "c1", "@1790000000 -0500");
    git_at(
        &repo,
        &["tag", "-a", "-m", "release 1.0", "v1.0"],
        "@1790000100 -0500",
    );

    write_commit(&repo, "a\nb\n", "c2", "@1790000200 -0500");
    git_ok(&repo, &["tag", "light-2"]);

    write_commit(&repo, "a\nb\nc\n", "c3", "@1790000300 -0500");
    git_at(
        &repo,
        &["tag", "-a", "-m", "release 2.0", "v2.0"],
        "@1790000400 -0500",
    );

    write_commit(&repo, "a\nb\nc\nd\n", "c4", "@1790000500 -0500");
    write_commit(&repo, "a\nb\nc\nd\ne\n", "c5", "@1790000600 -0500");

    (root, repo)
}

#[test]
fn describe_linear_history_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_linear_repo();

    for args in [
        // Default (annotated only) at various depths.
        vec!["describe"],
        vec!["describe", "HEAD"],
        vec!["describe", "HEAD~1"],
        vec!["describe", "HEAD~2"], // exactly v2.0
        vec!["describe", "HEAD~4"], // exactly v1.0
        vec!["describe", "--long"],
        vec!["describe", "--long", "HEAD~2"],
        // Lightweight tags require --tags; light-2 is closer than v2.0 at HEAD~3.
        vec!["describe", "--tags"],
        vec!["describe", "--tags", "HEAD~3"],
        vec!["describe", "--tags", "--long"],
        vec!["describe", "--tags", "HEAD~3", "--long"],
        // Abbreviation controls.
        vec!["describe", "--abbrev=4"],
        vec!["describe", "--abbrev=12"],
        vec!["describe", "--abbrev=0"],
        vec!["describe", "--abbrev=40"],
        vec!["describe", "--abbrev=0", "HEAD~2"],
        // Minimum/clamped abbreviation lengths.
        vec!["describe", "--abbrev=1"],
        vec!["describe", "--abbrev=3"],
        // Exact match (an alias for --candidates=0).
        vec!["describe", "--exact-match", "HEAD~2"],
        vec!["describe", "--exact-match"],
        vec!["describe", "--tags", "--exact-match", "HEAD~3"],
        // Candidate window.
        vec!["describe", "--candidates=1"],
        vec!["describe", "--tags", "--candidates=10"],
        vec!["describe", "--candidates=0"],
        vec!["describe", "--candidates=0", "HEAD~2"], // exact match satisfied
        // --candidates=0 / --exact-match error even under --always.
        vec!["describe", "--candidates=0", "--always"],
        vec!["describe", "--exact-match", "--always"],
        // --all uses ref-prefixed names.
        vec!["describe", "--all"],
        vec!["describe", "--all", "HEAD~2"],
        vec!["describe", "--all", "--long"],
        // Match / exclude filters.
        vec!["describe", "--match", "v1.*"],
        vec!["describe", "--tags", "--match", "light-*"],
        vec!["describe", "--exclude", "v2*"],
        // Multiple commit-ishes are described in order.
        vec!["describe", "HEAD~2", "HEAD~4"],
        // Conflicting options and unknown flags/switches.
        vec!["describe", "--long", "--abbrev=0"],
        vec!["describe", "--long", "--no-abbrev"],
        vec!["describe", "--bogus-flag"],
        vec!["describe", "-z"],
        // Invalid numeric option values.
        vec!["describe", "--abbrev=abc"],
        vec!["describe", "--candidates=abc"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

/// Tie-breaking: two annotated tags at the same commit resolve to the one with
/// the newer tagger date; equal dates fall back to the lexicographically smaller
/// tag name.
#[test]
fn describe_tag_tiebreak_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("describe-tiebreak");
    let repo = root.join("repo");
    git_ok(
        &root,
        &["init", "-q", "-b", "main", repo.to_str().expect("utf8")],
    );

    write_commit(&repo, "a\n", "c1", "@1790000000 -0500");
    // Two annotated tags at c1: `early` then `late`, with `late` newer.
    git_at(
        &repo,
        &["tag", "-a", "-m", "early", "early-tag"],
        "@1790000100 -0500",
    );
    git_at(
        &repo,
        &["tag", "-a", "-m", "late", "late-tag"],
        "@1790000200 -0500",
    );
    // Two annotated tags at c1 with identical dates: name tiebreak picks `aaa`.
    git_at(&repo, &["tag", "-a", "-m", "z", "zzz"], "@1790000300 -0500");
    git_at(&repo, &["tag", "-a", "-m", "a", "aaa"], "@1790000300 -0500");

    write_commit(&repo, "a\nb\n", "c2", "@1790000400 -0500");

    for args in [
        vec!["describe"],
        vec!["describe", "--long"],
        vec!["describe", "--exact-match", "HEAD~1"],
        vec!["describe", "--candidates=1"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

/// Merge history: depth counts every commit reachable from the target but not the
/// tag, and `--first-parent` restricts the walk to the first-parent chain.
#[test]
fn describe_merge_history_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("describe-merge");
    let repo = root.join("repo");
    git_ok(
        &root,
        &["init", "-q", "-b", "main", repo.to_str().expect("utf8")],
    );

    write_commit(&repo, "base\n", "base", "@1790000000 -0500");
    git_at(
        &repo,
        &["tag", "-a", "-m", "base", "basetag"],
        "@1790000050 -0500",
    );

    // Branches touch disjoint files so the merge is conflict-free; depth still
    // counts side1, side2, main1 and the merge commit (4) from basetag.
    git_ok(&repo, &["checkout", "-q", "-b", "side"]);
    write_named_commit(&repo, "side.txt", "side1\n", "side1", "@1790000100 -0500");
    git_at(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "side2"],
        "@1790000200 -0500",
    );

    git_ok(&repo, &["checkout", "-q", "main"]);
    write_named_commit(&repo, "main.txt", "main1\n", "main1", "@1790000300 -0500");
    git_at(
        &repo,
        &["merge", "-q", "--no-ff", "side", "-m", "merge"],
        "@1790000400 -0500",
    );

    for args in [
        vec!["describe"],
        vec!["describe", "--long"],
        vec!["describe", "--first-parent"],
        vec!["describe", "--long", "--first-parent"],
    ] {
        assert_same(&repo, &args);
    }

    let _ = fs::remove_dir_all(&root);
}

/// `--dirty`/`--broken` marks and the `--always` fallback, including the
/// no-names and unannotated-tag error paths.
#[test]
fn describe_dirty_always_and_errors_match_git() {
    if !git_available() {
        return;
    }

    // Repo with an annotated tag, used for --dirty and --always-with-tag.
    let tagged_root = unique_temp_dir("describe-dirty");
    let tagged = tagged_root.join("repo");
    git_ok(
        &tagged_root,
        &["init", "-q", "-b", "main", tagged.to_str().expect("utf8")],
    );
    write_commit(&tagged, "a\n", "c1", "@1790000000 -0500");
    git_at(
        &tagged,
        &["tag", "-a", "-m", "v1", "v1.0"],
        "@1790000100 -0500",
    );
    write_commit(&tagged, "a\nb\n", "c2", "@1790000200 -0500");

    // Clean working tree first.
    assert_same(&tagged, &["describe", "--dirty"]);
    assert_same(&tagged, &["describe", "--dirty=-WIP"]);

    // An untracked file must NOT count as dirty.
    fs::write(tagged.join("untracked.txt"), "x\n").expect("write untracked");
    assert_same(&tagged, &["describe", "--dirty"]);
    fs::remove_file(tagged.join("untracked.txt")).expect("rm untracked");

    // A tracked modification IS dirty.
    fs::write(tagged.join("f.txt"), "a\nb\nmodified\n").expect("modify f.txt");
    assert_same(&tagged, &["describe", "--dirty"]);
    assert_same(&tagged, &["describe", "--dirty=-dev"]);
    assert_same(&tagged, &["describe", "--long", "--dirty"]);
    // --dirty cannot be combined with a commit-ish.
    assert_same(&tagged, &["describe", "--dirty", "HEAD"]);
    // Restore the clean tree.
    git_ok(&tagged, &["checkout", "-q", "--", "f.txt"]);

    // Repo with only a lightweight tag: default mode suggests --tags.
    let light_root = unique_temp_dir("describe-light");
    let light = light_root.join("repo");
    git_ok(
        &light_root,
        &["init", "-q", "-b", "main", light.to_str().expect("utf8")],
    );
    write_commit(&light, "a\n", "c1", "@1790000000 -0500");
    git_ok(&light, &["tag", "lightweight"]);
    write_commit(&light, "a\nb\n", "c2", "@1790000200 -0500");

    assert_same(&light, &["describe"]); // "No annotated tags ... try --tags"
    assert_same(&light, &["describe", "--always"]);
    assert_same(&light, &["describe", "--tags"]);

    // Repo whose only annotated tag lives on an unrelated (unreachable) branch:
    // git reports "No tags can describe" rather than suggesting --tags.
    let unreach_root = unique_temp_dir("describe-unreach");
    let unreach = unreach_root.join("repo");
    git_ok(
        &unreach_root,
        &["init", "-q", "-b", "main", unreach.to_str().expect("utf8")],
    );
    write_named_commit(&unreach, "f.txt", "a\n", "c1", "@1790000000 -0500");
    git_ok(&unreach, &["checkout", "--orphan", "other", "-q"]);
    write_named_commit(&unreach, "g.txt", "x\n", "o1", "@1790000100 -0500");
    git_at(
        &unreach,
        &["tag", "-a", "-m", "unreach", "unreach-tag"],
        "@1790000150 -0500",
    );
    git_ok(&unreach, &["checkout", "-q", "main"]);
    write_named_commit(&unreach, "f.txt", "a\nb\n", "c2", "@1790000200 -0500");

    assert_same(&unreach, &["describe"]); // "No tags can describe ... Try --always"
    assert_same(&unreach, &["describe", "--always"]);

    // Repo with no tags at all: "No names found" and the --always fallback.
    let bare_root = unique_temp_dir("describe-notags");
    let bare = bare_root.join("repo");
    git_ok(
        &bare_root,
        &["init", "-q", "-b", "main", bare.to_str().expect("utf8")],
    );
    write_commit(&bare, "a\n", "c1", "@1790000000 -0500");

    assert_same(&bare, &["describe"]); // "No names found, cannot describe anything."
    assert_same(&bare, &["describe", "--always"]);
    assert_same(&bare, &["describe", "--always", "--dirty"]); // clean -> bare hash
    assert_same(&bare, &["describe", "bogus-ref"]); // No names found (no resolve)
    assert_same(&bare, &["describe", "--always", "bogus-ref"]); // Not a valid object name

    let _ = fs::remove_dir_all(&tagged_root);
    let _ = fs::remove_dir_all(&light_root);
    let _ = fs::remove_dir_all(&unreach_root);
    let _ = fs::remove_dir_all(&bare_root);
}
