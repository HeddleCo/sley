//! Differential interop tests for `git sparse-checkout` against the system
//! `git` binary.
//!
//! Each test builds two identical repositories — one driven by the system `git`,
//! one by `git-rs` — runs the same `sparse-checkout` invocations against each,
//! and asserts that stdout, stderr, the exit code, the generated
//! `info/sparse-checkout` pattern file, and the resulting set of worktree files
//! all match.

use std::collections::BTreeSet;
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

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_env(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, content).expect("write fixture file");
}

/// Creates a populated repository at `root` using the system `git` so both the
/// reference and the git-rs copy start from byte-identical history.
fn build_fixture(root: &Path) {
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    write_file(root, "top.txt", "top\n");
    write_file(root, "a/file.txt", "a\n");
    write_file(root, "a/b/file.txt", "ab\n");
    write_file(root, "a/b/c/file.txt", "abc\n");
    write_file(root, "c/file.txt", "c\n");
    write_file(root, "d/file.txt", "d\n");
    write_file(root, "z/y/file.txt", "zy\n");
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-qm", "init"]);
}

/// The set of tracked files currently present in the worktree (relative paths
/// with `/` separators), excluding the `.git` directory.
fn worktree_files(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    collect_files(root, root, &mut files);
    files
}

fn collect_files(root: &Path, dir: &Path, files: &mut BTreeSet<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            collect_files(root, &path, files);
        } else {
            let rel = path
                .strip_prefix(root)
                .expect("strip root prefix")
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(rel);
        }
    }
}

fn read_sparse_file(root: &Path) -> Option<Vec<u8>> {
    fs::read(root.join(".git").join("info").join("sparse-checkout")).ok()
}

/// Runs the same `sparse-checkout` invocation against both repositories and
/// asserts identical stdout, stderr, exit status, pattern file, and worktree
/// contents.
fn assert_parity(git_repo: &Path, rs_repo: &Path, args: &[&str]) {
    let expected = git(git_repo, args);
    let actual = git_rs(rs_repo, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit code differed for {args:?}\n git stderr: {}\n rs  stderr: {}",
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for {args:?}",
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for {args:?}",
    );
    assert_eq!(
        read_sparse_file(rs_repo),
        read_sparse_file(git_repo),
        "info/sparse-checkout differed after {args:?}",
    );
    assert_eq!(
        worktree_files(rs_repo),
        worktree_files(git_repo),
        "worktree files differed after {args:?}",
    );
}

/// Builds a paired (git, git-rs) repository under a fresh temp root.
fn paired_repos(name: &str) -> (PathBuf, PathBuf) {
    let root = unique_temp_dir(name);
    let git_repo = root.join("git");
    let rs_repo = root.join("rs");
    fs::create_dir_all(&git_repo).expect("create git repo dir");
    fs::create_dir_all(&rs_repo).expect("create rs repo dir");
    build_fixture(&git_repo);
    build_fixture(&rs_repo);
    (git_repo, rs_repo)
}

#[test]
fn cone_init_set_add_list_matches_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("cone-flow");
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "init", "--cone"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "a", "c"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "add", "d"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
}

#[test]
fn cone_set_generates_parent_guards_like_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("cone-nested");
    // Nested directories at mixed depths exercise the parent-guard / recursive
    // split and the slash-aware ordering of the generated pattern file.
    assert_parity(
        &git_repo,
        &rs_repo,
        &["sparse-checkout", "set", "a/b/c", "z/y", "d"],
    );
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
    // A parent directory subsumes its descendants.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "a", "a/b"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
}

#[test]
fn set_without_init_defaults_to_cone_like_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("cone-autoinit");
    // `set` on a fresh worktree implicitly initializes cone mode.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "a", "d"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
}

#[test]
fn non_cone_set_add_list_matches_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("noncone-flow");
    assert_parity(
        &git_repo,
        &rs_repo,
        &["sparse-checkout", "set", "--no-cone", "/a/", "/c/"],
    );
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "add", "/d/"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
}

#[test]
fn reapply_after_set_matches_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("reapply");
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "a"]);
    // Drop an out-of-cone file back into the worktree, then reapply should remove
    // it again identically in both implementations.
    write_file(&git_repo, "c/file.txt", "c\n");
    write_file(&rs_repo, "c/file.txt", "c\n");
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "reapply"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
}

#[test]
fn disable_restores_full_worktree_like_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("disable");
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "a"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "disable"]);
    // After disable, the worktree is fully expanded again.
    assert_eq!(
        worktree_files(&rs_repo),
        worktree_files(&git_repo),
        "worktree files differed after disable",
    );
}

#[test]
fn error_paths_match_git() {
    if !git_available() {
        return;
    }
    let (git_repo, rs_repo) = paired_repos("errors");
    // No subcommand / unknown subcommand.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "bogus"]);
    // Operations that require an active sparse-checkout.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "list"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "add", "a"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "reapply"]);
    // Cone mode rejects leading-slash "patterns" where a directory is expected.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "init", "--cone"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "/a/"]);
    // Unknown options surface git's option-help block verbatim.
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "init", "--bogus"]);
    assert_parity(&git_repo, &rs_repo, &["sparse-checkout", "set", "--bogus"]);
}
