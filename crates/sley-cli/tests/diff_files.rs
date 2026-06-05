//! Differential interop tests for `git diff-files` against the system `git`.
//!
//! `diff-files` is plumbing: it compares the working tree against the index, and
//! its defaults differ from porcelain `git diff` (raw output by default, full
//! object names in raw mode). Each test drives both binaries with identical
//! arguments in identical repositories and asserts byte-for-byte stdout, stderr,
//! and exit-code parity. The whole suite is skipped when `git` is unavailable.

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

/// Run a program with the fixed identity/date environment so commit and object
/// ids are reproducible across both `git` and `sley`.
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
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
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

/// Assert that `sley` matches `git` on stdout, exit code, and stderr for the
/// given argument vector run in `cwd`.
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
        "exit code differs for {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {args:?}"
    );
}

/// Build a repo whose working tree diverges from the index across the common
/// change classes: a content modification, a deletion, a binary change, and a
/// nested-path modification. The returned path is the repository root.
fn setup_mixed_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().expect("utf8 path")]);
    fs::write(repo.join("a.txt"), "line1\nline2\nline3\n").expect("write a.txt");
    fs::write(repo.join("b.txt"), "keep\n").expect("write b.txt");
    fs::create_dir(repo.join("sub")).expect("mkdir sub");
    fs::write(repo.join("sub/c.txt"), "nested\n").expect("write sub/c.txt");
    fs::write(repo.join("bin.dat"), b"bin\x00data\n").expect("write bin.dat");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Diverge the working tree from the index.
    fs::write(repo.join("a.txt"), "line1\nlineX\nline3\nline4\n").expect("modify a.txt");
    fs::remove_file(repo.join("b.txt")).expect("delete b.txt");
    fs::write(repo.join("sub/c.txt"), "nested-changed\n").expect("modify sub/c.txt");
    fs::write(repo.join("bin.dat"), b"bin\x00DATA2\n").expect("modify bin.dat");
    repo
}

/// The plumbing defaults: bare `diff-files` is diff-raw with full object names,
/// and `-z` switches to NUL-terminated raw records.
#[test]
fn diff_files_default_raw_matches_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-raw");
    assert_same(&repo, &["diff-files"]);
    assert_same(&repo, &["diff-files", "--raw"]);
    assert_same(&repo, &["diff-files", "-z"]);
    assert_same(&repo, &["diff-files", "--abbrev"]);
    assert_same(&repo, &["diff-files", "--abbrev=12"]);
    assert_same(&repo, &["diff-files", "--no-abbrev"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// Patch output, including combinations that prepend raw/stat blocks and the
/// full-index variant.
#[test]
fn diff_files_patch_matches_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-patch");
    assert_same(&repo, &["diff-files", "-p"]);
    assert_same(&repo, &["diff-files", "-u"]);
    assert_same(&repo, &["diff-files", "--patch"]);
    assert_same(&repo, &["diff-files", "-p", "--full-index"]);
    assert_same(&repo, &["diff-files", "-p", "--abbrev=20"]);
    assert_same(&repo, &["diff-files", "--patch-with-raw"]);
    assert_same(&repo, &["diff-files", "--patch-with-stat"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// Name-only / name-status modes, both newline- and NUL-delimited.
#[test]
fn diff_files_name_modes_match_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-name");
    assert_same(&repo, &["diff-files", "--name-only"]);
    assert_same(&repo, &["diff-files", "--name-status"]);
    assert_same(&repo, &["diff-files", "--name-only", "-z"]);
    assert_same(&repo, &["diff-files", "--name-status", "-z"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// Stat-family output: `--stat`, `--numstat`, `--shortstat`, `--compact-summary`,
/// and `--summary`, including the binary-file rows.
#[test]
fn diff_files_stat_modes_match_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-stat");
    assert_same(&repo, &["diff-files", "--stat"]);
    assert_same(&repo, &["diff-files", "--numstat"]);
    assert_same(&repo, &["diff-files", "--shortstat"]);
    assert_same(&repo, &["diff-files", "--compact-summary"]);
    assert_same(&repo, &["diff-files", "--summary"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// Pathspec narrowing: a single file, an explicit list, and a directory prefix.
/// Output paths are reported relative to the repository root regardless of cwd.
#[test]
fn diff_files_pathspec_matches_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-pathspec");
    assert_same(&repo, &["diff-files", "--", "a.txt"]);
    assert_same(
        &repo,
        &["diff-files", "--name-only", "--", "a.txt", "sub/c.txt"],
    );
    assert_same(&repo, &["diff-files", "--name-only", "--", "sub"]);
    assert_same(&repo, &["diff-files", "--name-only", "--", "nonexistent"]);
    // From a subdirectory, paths still print relative to the repo root.
    let sub = repo.join("sub");
    assert_same(&sub, &["diff-files", "--name-only"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// `--diff-filter` selection, including lowercase exclusion classes, over a repo
/// that has both a modification and a deletion.
#[test]
fn diff_files_diff_filter_matches_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-filter");
    assert_same(&repo, &["diff-files", "--diff-filter=M", "--name-status"]);
    assert_same(&repo, &["diff-files", "--diff-filter=D", "--name-status"]);
    assert_same(&repo, &["diff-files", "--diff-filter=d", "--name-status"]);
    assert_same(&repo, &["diff-files", "--diff-filter=AMD", "--name-status"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// Exit-code contract: `--quiet`/`--exit-code`/`-s --exit-code` return 1 when the
/// working tree differs from the index and 0 when it matches. `-q` (silent for
/// nonexistent files) is *not* `--quiet`: it prints normally and exits 0.
#[test]
fn diff_files_exit_codes_match_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-exit");
    // Dirty working tree.
    assert_same(&repo, &["diff-files", "--quiet"]);
    assert_same(&repo, &["diff-files", "--exit-code"]);
    assert_same(&repo, &["diff-files", "-s", "--exit-code"]);
    assert_same(&repo, &["diff-files", "-q"]);
    assert_same(&repo, &["diff-files", "-q", "--exit-code"]);

    // Clean working tree: stage everything so index == worktree.
    git_ok(&repo, &["add", "-A"]);
    assert_same(&repo, &["diff-files"]);
    assert_same(&repo, &["diff-files", "--quiet"]);
    assert_same(&repo, &["diff-files", "--exit-code"]);
    assert_same(&repo, &["diff-files", "-p"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// A mode-only change (chmod +x with identical content) must reproduce git's
/// raw, patch, name-status, and summary rendering of an executable-bit flip.
#[cfg(unix)]
#[test]
fn diff_files_mode_change_matches_git() {
    if !git_available() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = unique_temp_dir("diff-files-mode");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().expect("utf8 path")]);
    fs::write(repo.join("s.sh"), "#!/bin/sh\necho hi\n").expect("write s.sh");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    let path = repo.join("s.sh");
    let mut perms = fs::metadata(&path).expect("stat s.sh").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod s.sh");

    assert_same(&repo, &["diff-files"]);
    assert_same(&repo, &["diff-files", "-p"]);
    assert_same(&repo, &["diff-files", "--name-status"]);
    assert_same(&repo, &["diff-files", "--summary"]);
    assert_same(&repo, &["diff-files", "--stat"]);
    fs::remove_dir_all(&root).ok();
}

/// An unstaged rename (file moved on disk but the original still in the index)
/// is detected by default and with `-M`, and reported as delete+add under
/// `--no-renames`.
#[test]
fn diff_files_rename_detection_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-files-rename");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().expect("utf8 path")]);
    fs::write(repo.join("orig.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").expect("write orig.txt");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Move on disk only; the index still records orig.txt.
    fs::remove_file(repo.join("orig.txt")).expect("remove orig.txt");
    fs::write(repo.join("renamed.txt"), "a\nb\nc\nd\ne\nf\ng\nH\n").expect("write renamed.txt");

    assert_same(&repo, &["diff-files", "--name-status"]);
    assert_same(&repo, &["diff-files", "-M", "--name-status"]);
    assert_same(&repo, &["diff-files", "-M", "-p"]);
    assert_same(&repo, &["diff-files", "--no-renames", "--name-status"]);
    fs::remove_dir_all(&root).ok();
}

/// Argument-handling parity: `-h` prints usage to stdout, an unknown option
/// prints usage to stderr (both exit 129), and conflicting name selectors are a
/// fatal error (exit 128).
#[test]
fn diff_files_usage_and_errors_match_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-usage");
    assert_same(&repo, &["diff-files", "-h"]);
    assert_same(&repo, &["diff-files", "--this-is-not-an-option"]);
    assert_same(&repo, &["diff-files", "--name-only", "--name-status"]);
    assert_same(&repo, &["diff-files", "-s", "--name-status"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}

/// `-R` swaps the pair in the name-oriented modes (added becomes deleted and
/// vice versa), matching the crate's `diff`/`diff-index` reverse handling.
#[test]
fn diff_files_reverse_name_modes_match_git() {
    if !git_available() {
        return;
    }
    let repo = setup_mixed_repo("diff-files-reverse");
    assert_same(&repo, &["diff-files", "-R", "--name-status"]);
    assert_same(&repo, &["diff-files", "-R", "--name-only"]);
    fs::remove_dir_all(repo.parent().expect("repo has parent")).ok();
}
