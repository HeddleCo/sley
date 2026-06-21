//! Differential interop tests for `git diff-index` vs the system `git` binary.
//!
//! Each test builds a fixture repository with a deterministic identity/date and
//! asserts that `sley diff-index` produces byte-identical stdout (and matching
//! exit codes / stderr for the error and usage paths) to real `git`. The whole
//! suite is gated on `git --version` succeeding so it is a no-op where git is
//! unavailable.

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
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    assert!(git(cwd, args).status.success(), "git {args:?} failed");
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

/// Assert that `sley` and `git` produce identical stdout and exit code. stdout
/// is compared as raw bytes so NUL-delimited (`-z`) and binary-file output is
/// checked exactly, with a lossy rendering in the failure message.
fn assert_same(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(
        r.stdout,
        g.stdout,
        "stdout differs for {args:?}\n  sley: {:?}\n  git:    {:?}\n  sley stderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        String::from_utf8_lossy(&r.stderr),
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}\n  sley stderr: {}",
        String::from_utf8_lossy(&r.stderr),
    );
}

/// Like [`assert_same`] but also asserts identical stderr — used for usage and
/// fatal-error paths where the message and stream matter.
fn assert_same_all(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(
        r.stdout,
        g.stdout,
        "stdout differs for {args:?}\n  sley: {:?}\n  git: {:?}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {args:?}",
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}",
    );
}

fn assert_sley_stdout(cwd: &Path, args: &[&str], expected: &str) {
    let r = git_rs(cwd, args);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        expected,
        "stdout differs for {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr),
    );
    assert!(
        r.status.success(),
        "sley {args:?} failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(
        r.stderr.is_empty(),
        "stderr not empty for {args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
}

/// Initialise a fresh repo at `<root>/repo` with the default object format.
fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    git_ok(
        root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    repo
}

/// All the output-format flags share the same change set; default output is the
/// raw listing (full oids), matching the plumbing command rather than `git diff`.
#[test]
fn diff_index_output_modes_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-modes");
    let repo = init_repo(&root);
    fs::write(repo.join("f1.txt"), "a\nb\nc\n").expect("test operation should succeed");
    fs::write(repo.join("f2.txt"), "keep\n").expect("test operation should succeed");
    fs::create_dir(repo.join("sub")).expect("test operation should succeed");
    fs::write(repo.join("sub/nested.txt"), "deep\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);

    // Stage a modify, a delete, and an add.
    fs::write(repo.join("f1.txt"), "a\nB\nc\nd\n").expect("test operation should succeed");
    fs::remove_file(repo.join("f2.txt")).expect("test operation should succeed");
    fs::write(repo.join("f3.txt"), "new\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    for flags in [
        &["diff-index", "--cached", "HEAD"][..],
        &["diff-index", "--raw", "--cached", "HEAD"][..],
        &["diff-index", "-p", "--cached", "HEAD"][..],
        &["diff-index", "-u", "--cached", "HEAD"][..],
        &["diff-index", "--patch", "--cached", "HEAD"][..],
        &["diff-index", "--name-status", "--cached", "HEAD"][..],
        &["diff-index", "--name-only", "--cached", "HEAD"][..],
        &["diff-index", "--stat", "--cached", "HEAD"][..],
        &["diff-index", "--compact-summary", "--cached", "HEAD"][..],
        &["diff-index", "--numstat", "--cached", "HEAD"][..],
        &["diff-index", "--shortstat", "--cached", "HEAD"][..],
        &["diff-index", "--summary", "--cached", "HEAD"][..],
        &["diff-index", "--patch-with-raw", "--cached", "HEAD"][..],
        &["diff-index", "--patch-with-stat", "--cached", "HEAD"][..],
    ] {
        assert_same(&repo, flags);
    }

    fs::remove_dir_all(&root).ok();
}

/// `-z` switches every textual output to NUL-delimited records; check the
/// formats that change.
#[test]
fn diff_index_nul_terminated_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-z");
    let repo = init_repo(&root);
    fs::write(repo.join("a.txt"), "one\n").expect("test operation should succeed");
    fs::write(repo.join("b.txt"), "two\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("a.txt"), "one\nmore\n").expect("test operation should succeed");
    fs::remove_file(repo.join("b.txt")).expect("test operation should succeed");
    fs::write(repo.join("c.txt"), "three\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "-z", "--cached", "HEAD"]);
    assert_same(
        &repo,
        &["diff-index", "-z", "--name-status", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "-z", "--name-only", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "-z", "--numstat", "--cached", "HEAD"],
    );

    fs::remove_dir_all(&root).ok();
}

/// Without `--cached` the comparison is against the working tree, and the
/// new-side oid is zeroed for paths whose worktree contents differ from the
/// index (while still showing the real oid when index and worktree agree).
#[test]
fn diff_index_worktree_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-worktree");
    let repo = init_repo(&root);
    fs::write(repo.join("tracked.txt"), "v1\n").expect("test operation should succeed");
    fs::write(repo.join("staged.txt"), "s1\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);

    // staged.txt: staged change (index == worktree -> real oid).
    fs::write(repo.join("staged.txt"), "s2\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "staged.txt"]);
    // tracked.txt: unstaged worktree edit (index != worktree -> zeroed oid).
    fs::write(repo.join("tracked.txt"), "v1\nworktree-only\n")
        .expect("test operation should succeed");

    assert_same(&repo, &["diff-index", "HEAD"]);
    assert_same(&repo, &["diff-index", "--raw", "HEAD"]);
    assert_same(&repo, &["diff-index", "-p", "HEAD"]);
    assert_same(&repo, &["diff-index", "--name-status", "HEAD"]);
    assert_same(&repo, &["diff-index", "--stat", "HEAD"]);
    assert_same(&repo, &["diff-index", "--numstat", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

/// Rename/copy detection is opt-in (`diff.renames` is intentionally ignored):
/// the default reports delete+add, `-M` reports `R100`, and `-C` implies rename
/// detection too. `--find-copies-harder` additionally detects copies from
/// unmodified sources.
#[test]
fn diff_index_rename_copy_detection_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-rename");
    let repo = init_repo(&root);
    fs::write(repo.join("orig.txt"), "line1\nline2\nline3\nline4\nline5\n")
        .expect("test operation should succeed");
    fs::write(repo.join("keep.txt"), "alpha\nbeta\ngamma\n")
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);

    git_ok(&repo, &["mv", "orig.txt", "renamed.txt"]);
    fs::copy(repo.join("keep.txt"), repo.join("copy.txt")).expect("test operation should succeed");
    git_ok(&repo, &["add", "copy.txt"]);

    // diff.renames=true must NOT make plumbing diff-index detect renames.
    git_ok(&repo, &["config", "diff.renames", "true"]);

    assert_same(&repo, &["diff-index", "--cached", "--name-status", "HEAD"]);
    assert_same(&repo, &["diff-index", "--cached", "HEAD"]);
    assert_same(
        &repo,
        &["diff-index", "-M", "--cached", "--name-status", "HEAD"],
    );
    assert_same(&repo, &["diff-index", "-M", "--cached", "HEAD"]);
    assert_same(
        &repo,
        &["diff-index", "-M50", "--cached", "--name-status", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "-C", "--cached", "--name-status", "HEAD"],
    );
    assert_same(
        &repo,
        &[
            "diff-index",
            "-C",
            "--find-copies-harder",
            "--cached",
            "--name-status",
            "HEAD",
        ],
    );
    assert_same(
        &repo,
        &[
            "diff-index",
            "-C",
            "--find-copies-harder",
            "-p",
            "--cached",
            "HEAD",
        ],
    );
    assert_same(
        &repo,
        &[
            "diff-index",
            "--no-renames",
            "--cached",
            "--name-status",
            "HEAD",
        ],
    );

    fs::remove_dir_all(&root).ok();
}

/// `-R` swaps the file pairs and `--diff-filter` selects entries by status.
#[test]
fn diff_index_reverse_and_filter_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-filter");
    let repo = init_repo(&root);
    fs::write(repo.join("mod.txt"), "x\n").expect("test operation should succeed");
    fs::write(repo.join("del.txt"), "y\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("mod.txt"), "x\nz\n").expect("test operation should succeed");
    fs::remove_file(repo.join("del.txt")).expect("test operation should succeed");
    fs::write(repo.join("add.txt"), "w\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "-R", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "-R", "-p", "--cached", "HEAD"]);
    // `-R` also swaps the patch prefixes, including custom ones.
    assert_same(
        &repo,
        &[
            "diff-index",
            "-R",
            "--src-prefix=X/",
            "--dst-prefix=Y/",
            "-p",
            "--cached",
            "HEAD",
        ],
    );
    assert_same(
        &repo,
        &["diff-index", "--diff-filter=A", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "--diff-filter=D", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "--diff-filter=M", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &[
            "diff-index",
            "--diff-filter=AM",
            "--cached",
            "--name-status",
            "HEAD",
        ],
    );

    fs::remove_dir_all(&root).ok();
}

/// Object-name abbreviation: the raw listing shows full oids by default, `-c
/// core.abbrev` alone does not abbreviate it, `--abbrev`/`--abbrev=<n>` do, and
/// `--full-index` expands the patch index line.
#[test]
fn diff_index_abbrev_controls_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-abbrev");
    let repo = init_repo(&root);
    fs::write(repo.join("f.txt"), "a\nb\nc\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("f.txt"), "a\nB\nc\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--abbrev", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--abbrev=12", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--no-abbrev", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "-p", "--cached", "HEAD"]);
    assert_same(
        &repo,
        &["diff-index", "--full-index", "-p", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &["diff-index", "--no-abbrev", "-p", "--cached", "HEAD"],
    );
    // `-c core.abbrev` affects the patch index line but not the raw listing.
    assert_same(
        &repo,
        &["-c", "core.abbrev=10", "diff-index", "--cached", "HEAD"],
    );
    assert_same(
        &repo,
        &[
            "-c",
            "core.abbrev=10",
            "diff-index",
            "--abbrev",
            "--cached",
            "HEAD",
        ],
    );
    assert_same(
        &repo,
        &[
            "-c",
            "core.abbrev=10",
            "diff-index",
            "-p",
            "--cached",
            "HEAD",
        ],
    );

    fs::remove_dir_all(&root).ok();
}

/// Pathspec restriction works from the repository root and from a subdirectory.
#[test]
fn diff_index_pathspec_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-pathspec");
    let repo = init_repo(&root);
    fs::create_dir(repo.join("dir")).expect("test operation should succeed");
    fs::write(repo.join("top.txt"), "t\n").expect("test operation should succeed");
    fs::write(repo.join("dir/inner.txt"), "i\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("top.txt"), "t\nt2\n").expect("test operation should succeed");
    fs::write(repo.join("dir/inner.txt"), "i\ni2\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "--cached", "HEAD", "--", "dir"]);
    assert_same(&repo, &["diff-index", "--cached", "HEAD", "--", "top.txt"]);

    // From the subdirectory, a relative pathspec is resolved against the cwd.
    let dir = repo.join("dir");
    assert_same(&dir, &["diff-index", "--cached", "HEAD", "--", "inner.txt"]);
    assert_same(&dir, &["diff-index", "--cached", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_index_cached_max_depth_limits_index_paths() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-max-depth");
    let repo = init_repo(&root);
    fs::write(repo.join("file"), "base\n").expect("test operation should succeed");
    fs::create_dir_all(repo.join("one/two/three")).expect("test operation should succeed");
    fs::write(repo.join("one/file"), "base\n").expect("test operation should succeed");
    fs::write(repo.join("one/two/file"), "base\n").expect("test operation should succeed");
    fs::write(repo.join("one/two/three/file"), "base\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    for path in ["file", "one/file", "one/two/file", "one/two/three/file"] {
        fs::write(repo.join(path), "index\n").expect("test operation should succeed");
    }
    git_ok(&repo, &["add", "."]);

    assert_sley_stdout(
        &repo,
        &[
            "diff-index",
            "--max-depth=0",
            "--name-only",
            "--cached",
            "HEAD",
            "--",
        ],
        "file\n",
    );
    assert_sley_stdout(
        &repo,
        &[
            "diff-index",
            "--max-depth=1",
            "--name-only",
            "--cached",
            "HEAD",
            "--",
        ],
        "file\none/file\n",
    );
    assert_sley_stdout(
        &repo,
        &[
            "diff-index",
            "--max-depth=0",
            "--name-only",
            "--cached",
            "HEAD",
            "--",
            "one",
        ],
        "",
    );
    assert_sley_stdout(
        &repo,
        &[
            "diff-index",
            "--max-depth=2",
            "--name-only",
            "--cached",
            "HEAD",
            "--",
            "one",
        ],
        "one/file\none/two/file\n",
    );
    assert_sley_stdout(
        &repo,
        &[
            "diff-index",
            "--max-depth=-1",
            "--name-only",
            "--cached",
            "HEAD",
            "--",
        ],
        "file\none/file\none/two/file\none/two/three/file\n",
    );

    fs::remove_dir_all(&root).ok();
}

/// A tag or raw tree oid resolves the same as a commit-ish, and exit codes for
/// `--exit-code`/`--quiet` are 1 when there are differences and 0 otherwise.
#[test]
fn diff_index_treeish_and_exit_codes_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-treeish");
    let repo = init_repo(&root);
    fs::write(repo.join("a.txt"), "a\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["tag", "v1"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();

    // No staged changes: exit 0 and empty output everywhere.
    assert_same(&repo, &["diff-index", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--exit-code", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--quiet", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--cached", "v1"]);
    assert_same(&repo, &["diff-index", "--cached", &tree]);

    // Stage a change: exit code becomes 1 for --exit-code/--quiet.
    fs::write(repo.join("a.txt"), "a\nb\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    assert_same(&repo, &["diff-index", "--exit-code", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--quiet", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--cached", "v1"]);
    assert_same(&repo, &["diff-index", "--cached", &tree]);

    fs::remove_dir_all(&root).ok();
}

/// The canonical empty tree is accepted even though the object is not stored,
/// so `diff-index --cached <empty-tree>` reports every staged path as added.
#[test]
fn diff_index_empty_tree_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-empty-tree");
    let repo = init_repo(&root);
    fs::write(repo.join("only.txt"), "content\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "only.txt"]);
    let empty = git(&repo, &["hash-object", "-t", "tree", "/dev/null"]);
    let empty = String::from_utf8_lossy(&empty.stdout).trim().to_string();

    assert_same(&repo, &["diff-index", "--cached", &empty]);
    assert_same(&repo, &["diff-index", "-p", "--cached", &empty]);
    assert_same(&repo, &["diff-index", "--name-status", "--cached", &empty]);

    fs::remove_dir_all(&root).ok();
}

/// Binary blobs render the `Binary files ... differ` patch and `-`/`-` numstat.
#[test]
fn diff_index_binary_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-binary");
    let repo = init_repo(&root);
    fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 255, 0, 7])
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("bin.dat"), [0u8, 1, 9, 9, 255, 0, 7, 42])
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "-p", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--numstat", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--stat", "--cached", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

/// Paths containing spaces are C-quoted identically in the raw, name-status, and
/// patch outputs.
#[test]
fn diff_index_quoted_paths_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-quoting");
    let repo = init_repo(&root);
    fs::write(repo.join("spaced name.txt"), "a b\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("spaced name.txt"), "a b\nc d\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);

    assert_same(&repo, &["diff-index", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "--name-status", "--cached", "HEAD"]);
    assert_same(&repo, &["diff-index", "-p", "--cached", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

/// Usage and error paths: missing tree-ish prints usage to stderr (exit 129),
/// `-h` prints the same usage to stdout (exit 129), and an unresolvable revision
/// is a fatal error (exit 128) with git's `ambiguous argument` message.
#[test]
fn diff_index_usage_and_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-index-usage");
    let repo = init_repo(&root);
    fs::write(repo.join("a.txt"), "a\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);

    assert_same_all(&repo, &["diff-index"]);
    assert_same_all(&repo, &["diff-index", "--cached"]);
    assert_same_all(&repo, &["diff-index", "-h"]);
    assert_same_all(&repo, &["diff-index", "--bogus", "--cached", "HEAD"]);
    assert_same_all(&repo, &["diff-index", "-Mfoo", "--cached", "HEAD"]);
    assert_same_all(
        &repo,
        &["diff-index", "--find-renames=foo", "--cached", "HEAD"],
    );
    assert_same_all(&repo, &["diff-index", "-Cfoo", "--cached", "HEAD"]);
    assert_same_all(
        &repo,
        &["diff-index", "--find-copies=foo", "--cached", "HEAD"],
    );
    assert_same_all(&repo, &["diff-index", "--cached", "definitely-not-a-ref"]);
    assert_same_all(&repo, &["diff-index", "--cached", "HEAD~9"]);

    fs::remove_dir_all(&root).ok();
}
