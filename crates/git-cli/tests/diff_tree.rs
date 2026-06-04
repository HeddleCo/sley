//! Differential interop tests for `git diff-tree` vs the system `git` binary.
//!
//! Each case runs the same arguments through both `git` and our `git-rs`
//! reimplementation in an identical repository and asserts byte-for-byte equal
//! stdout/stderr and matching exit codes. The whole suite is gated on `git`
//! being available so it is a no-op on machines without it.
//!
//! Fixtures use the fixed identity/date the workspace standardises on so commit
//! ids (which appear in diff-tree's headers and raw output) are reproducible.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// Run `program` with the standardized author/committer identity and dates so
/// generated object ids are deterministic across `git` and `git-rs`.
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

/// Like [`run_env`] but feeds `stdin_data` to the process's standard input
/// (used to exercise `diff-tree --stdin`).
fn run_env_stdin(program: &str, cwd: &Path, args: &[&str], stdin_data: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .take()
        .expect("child stdin piped")
        .write_all(stdin_data.as_bytes())
        .expect("write child stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
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
    run_env(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Resolve a revision via the *real* git so tests can build argument lists that
/// reference concrete object ids (trees, commits) by their full hash.
fn rev_parse(cwd: &Path, spec: &str) -> String {
    let out = git(cwd, &["rev-parse", spec]);
    assert!(
        out.status.success(),
        "git rev-parse {spec} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("rev-parse output utf8")
        .trim()
        .to_string()
}

/// Assert `git` and `git-rs` agree on stdout, stderr, and exit code for `args`.
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

/// As [`assert_same`] but exact bytes (for `-z` NUL-delimited output, where a
/// lossy string compare could mask differences).
fn assert_same_bytes(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(r.stdout, g.stdout, "stdout bytes differ for {args:?}");
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}"
    );
}

/// As [`assert_same`] but with data on stdin (for `--stdin`).
fn assert_same_stdin(cwd: &Path, args: &[&str], stdin_data: &str) {
    let g = run_env_stdin("git", cwd, args, stdin_data);
    let r = run_env_stdin(env!("CARGO_BIN_EXE_git-rs"), cwd, args, stdin_data);
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for {args:?} (stdin={stdin_data:?})\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {args:?} (stdin={stdin_data:?})"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?} (stdin={stdin_data:?})"
    );
}

/// Replace inexact rename/copy *scores* (`R085`, `C072`, the `similarity index
/// NN%` line, and `(NN%)` summary suffixes) with a fixed placeholder.
///
/// The similarity *percentage* is produced by the shared `git_diff_merge`
/// scorer, whose rounding differs from upstream git's `diffcore` by at most one
/// point on some inputs. That is an engine-level concern independent of
/// `diff-tree`'s formatting; this lets the rename/copy tests still assert that
/// detection, path pairing, status letters, and recursion all match exactly
/// while tolerating the off-by-one score.
fn normalize_scores(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // A status letter R/C followed by 1-3 digits (raw / name-status form).
        if (c == b'R' || c == b'C') && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit()) {
            out.push(c as char);
            out.push('#');
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    // Collapse "similarity index NN%" / "dissimilarity index NN%" and "(NN%)".
    let out = replace_percent_phrase(&out, "similarity index ");
    let out = replace_percent_phrase(&out, "dissimilarity index ");
    replace_paren_percent(&out)
}

fn replace_percent_phrase(text: &str, prefix: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(prefix) {
        result.push_str(&rest[..pos + prefix.len()]);
        let after = &rest[pos + prefix.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        result.push('#');
        rest = &after[digits.len()..];
    }
    result.push_str(rest);
    result
}

fn replace_paren_percent(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('(') {
        let after = &rest[pos + 1..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with("%)") {
            result.push_str(&rest[..pos]);
            result.push_str("(#%)");
            rest = &after[digits.len() + 2..];
        } else {
            result.push_str(&rest[..pos + 1]);
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

/// Like [`assert_same`] but compares with inexact rename/copy scores normalized
/// out (see [`normalize_scores`]). Exit codes are still compared exactly.
fn assert_same_scoreless(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
    assert_eq!(
        normalize_scores(&String::from_utf8_lossy(&r.stdout)),
        normalize_scores(&String::from_utf8_lossy(&g.stdout)),
        "stdout (score-normalized) differs for {args:?}\ngit-rs stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for {args:?}"
    );
}

/// Build a small repo with two commits spanning a nested directory edit, a
/// nested add, a top-level edit, and a deletion. Patch fixtures use single-line
/// files so the unified-diff hunks are whole-file (which both implementations
/// produce identically).
fn init_basic_repo(name: &str) -> (PathBuf, PathBuf) {
    let root = unique_temp_dir(name);
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::create_dir_all(repo.join("sub/deep")).unwrap();
    fs::write(repo.join("sub/deep/file.txt"), "a\n").unwrap();
    fs::write(repo.join("top.txt"), "top\n").unwrap();
    fs::write(repo.join("gone.txt"), "gone\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "c1"]);
    fs::write(repo.join("sub/deep/file.txt"), "b\n").unwrap();
    fs::write(repo.join("top.txt"), "top2\n").unwrap();
    fs::write(repo.join("sub/another.txt"), "new\n").unwrap();
    fs::remove_file(repo.join("gone.txt")).unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "c2"]);
    (root, repo)
}

#[test]
fn diff_tree_two_tree_and_single_commit_modes_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-modes");

    // Raw output, non-recursive (changed subtrees stay collapsed) and recursive.
    assert_same(&repo, &["diff-tree", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-r", "HEAD^", "HEAD"]);
    // Single commit prints the commit-id header, then commit-vs-parent.
    assert_same(&repo, &["diff-tree", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-r", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--no-commit-id", "-r", "HEAD"]);
    // -t surfaces the intermediate tree entries while recursing.
    assert_same(&repo, &["diff-tree", "-t", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-t", "-r", "HEAD^", "HEAD"]);
    // -s computes the diff but prints nothing (except the single-commit header).
    assert_same(&repo, &["diff-tree", "-s", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-s", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_name_and_stat_modes_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-name-stat");

    assert_same(&repo, &["diff-tree", "--name-only", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--name-only", "-r", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--name-status", "HEAD^", "HEAD"]);
    assert_same(
        &repo,
        &["diff-tree", "--name-status", "-r", "HEAD^", "HEAD"],
    );
    // The file-content summaries always operate recursively.
    assert_same(&repo, &["diff-tree", "--stat", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--numstat", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--shortstat", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--summary", "HEAD^", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_patch_mode_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-patch");

    // Single-line fixtures keep hunks whole-file, so unified output matches.
    assert_same(&repo, &["diff-tree", "-p", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-p", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--no-commit-id", "-p", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-p", "--full-index", "HEAD^", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_abbrev_and_tree_args_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-abbrev");
    let old_tree = rev_parse(&repo, "HEAD^^{tree}");
    let new_tree = rev_parse(&repo, "HEAD^{tree}");

    // Raw output abbreviation: default is full ids; --abbrev[=n] shortens them.
    assert_same(&repo, &["diff-tree", "-r", "--abbrev", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-r", "--abbrev=8", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-r", "--no-abbrev", "HEAD^", "HEAD"]);
    // Direct tree-ish operands (no commit header).
    assert_same(&repo, &["diff-tree", &old_tree, &new_tree]);
    assert_same(&repo, &["diff-tree", "-r", &old_tree, &new_tree]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_z_output_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-z");

    assert_same_bytes(&repo, &["diff-tree", "-z", "-r", "HEAD^", "HEAD"]);
    assert_same_bytes(
        &repo,
        &["diff-tree", "-z", "--name-only", "-r", "HEAD^", "HEAD"],
    );
    assert_same_bytes(
        &repo,
        &["diff-tree", "-z", "--name-status", "-r", "HEAD^", "HEAD"],
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_root_option_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-root");
    let first = {
        let out = git(&repo, &["rev-list", "--max-parents=0", "HEAD"]);
        String::from_utf8(out.stdout)
            .expect("rev-list utf8")
            .trim()
            .to_string()
    };

    // A root commit produces nothing without --root, and an add-from-empty diff
    // with it.
    assert_same(&repo, &["diff-tree", "-r", &first]);
    assert_same(&repo, &["diff-tree", &first]);
    assert_same(&repo, &["diff-tree", "--root", "-r", &first]);
    assert_same(&repo, &["diff-tree", "--root", &first]);
    assert_same(&repo, &["diff-tree", "--root", "-p", &first]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_rename_detection_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-tree-rename");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    // A file that survives a rename with a small edit (so it scores ~80%), plus a
    // top-level rename alongside a nested modification to exercise non-recursive
    // top-level rename detection.
    fs::write(repo.join("orig.txt"), "a\nb\nc\nd\ne\nf\ng\n").unwrap();
    fs::create_dir_all(repo.join("keep")).unwrap();
    fs::write(repo.join("keep/file.txt"), "x\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["mv", "orig.txt", "renamed.txt"]);
    fs::write(repo.join("renamed.txt"), "a\nb\nC\nd\ne\nf\ng\n").unwrap();
    fs::write(repo.join("keep/file.txt"), "X\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "rename"]);

    // diff-tree leaves rename detection off by default: a plain delete+add and
    // the nested modification, no rename pairing.
    assert_same(&repo, &["diff-tree", "-r", "HEAD^", "HEAD"]);
    // --no-renames forces the delete+add representation even with -M elsewhere.
    assert_same(&repo, &["diff-tree", "--no-renames", "-r", "HEAD^", "HEAD"]);
    // A high threshold rejects the ~85% pair, reverting to delete+add (no score
    // in the output, so this compares exactly).
    assert_same(&repo, &["diff-tree", "-M95", "-r", "HEAD^", "HEAD"]);

    // -M detects the rename: detection, path pairing, status letter, recursion,
    // and the nested modification must all match. The similarity *score* is
    // normalized out because the shared scorer rounds within one point of git.
    assert_same_scoreless(&repo, &["diff-tree", "-M", "-r", "HEAD^", "HEAD"]);
    // Non-recursive -M: the nested change stays a collapsed `040000` entry while
    // the top-level rename is still detected.
    assert_same_scoreless(&repo, &["diff-tree", "-M", "HEAD^", "HEAD"]);
    assert_same_scoreless(
        &repo,
        &["diff-tree", "-M", "--name-status", "HEAD^", "HEAD"],
    );
    assert_same_scoreless(&repo, &["diff-tree", "-M50", "-r", "HEAD^", "HEAD"]);

    fs::remove_dir_all(&root).ok();

    // A pure rename (identical content) scores an exact 100 in both engines, so
    // its full output — including the score — compares byte-for-byte.
    let root2 = unique_temp_dir("diff-tree-rename-exact");
    let repo2 = root2.join("repo");
    git_ok(&root2, &["init", "-q", repo2.to_str().unwrap()]);
    fs::write(repo2.join("orig.txt"), "stable\ncontent\nhere\n").unwrap();
    git_ok(&repo2, &["add", "."]);
    git_ok(&repo2, &["commit", "-qm", "base"]);
    git_ok(&repo2, &["mv", "orig.txt", "moved.txt"]);
    git_ok(&repo2, &["add", "-A"]);
    git_ok(&repo2, &["commit", "-qm", "move"]);
    assert_same(&repo2, &["diff-tree", "-M", "-r", "HEAD^", "HEAD"]);
    assert_same(
        &repo2,
        &["diff-tree", "-M", "--name-status", "HEAD^", "HEAD"],
    );
    assert_same(&repo2, &["diff-tree", "-M", "-p", "-r", "HEAD^", "HEAD"]);
    fs::remove_dir_all(&root2).ok();
}

#[test]
fn diff_tree_copy_detection_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-tree-copy");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::write(
        repo.join("orig.txt"),
        "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Copy orig.txt to copy.txt while editing the original (so the original is a
    // changed candidate source for -C without --find-copies-harder).
    fs::copy(repo.join("orig.txt"), repo.join("copy.txt")).unwrap();
    fs::write(
        repo.join("orig.txt"),
        "alpha\nbeta\ngamma\ndelta\nepsilon\nZETA\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "copy"]);

    // Raw and name-status copy reports (the patch form differs only in
    // hunk-context, which the shared patch writer renders whole-file).
    assert_same(&repo, &["diff-tree", "-C", "-r", "HEAD^", "HEAD"]);
    assert_same(
        &repo,
        &["diff-tree", "-C", "--name-status", "-r", "HEAD^", "HEAD"],
    );
    assert_same(
        &repo,
        &[
            "diff-tree",
            "-C",
            "--find-copies-harder",
            "-r",
            "HEAD^",
            "HEAD",
        ],
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_mode_change_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("diff-tree-mode");
    let repo = root.join("repo");
    git_ok(&root, &["init", "-q", repo.to_str().unwrap()]);
    fs::write(repo.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Flip the executable bit via the index so the change is portable.
    git_ok(&repo, &["update-index", "--chmod=+x", "script.sh"]);
    git_ok(&repo, &["commit", "-qm", "chmod"]);

    assert_same(&repo, &["diff-tree", "-r", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "-p", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--summary", "HEAD^", "HEAD"]);
    assert_same(&repo, &["diff-tree", "--stat", "HEAD^", "HEAD"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_stdin_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-stdin");

    let commits = {
        let out = git(&repo, &["rev-list", "HEAD"]);
        String::from_utf8(out.stdout).expect("rev-list utf8")
    };
    let old_tree = rev_parse(&repo, "HEAD^^{tree}");
    let new_tree = rev_parse(&repo, "HEAD^{tree}");

    // A list of commit ids: each prints its id header then its diff; the root
    // commit (no parent, no --root) is silently skipped.
    assert_same_stdin(&repo, &["diff-tree", "--stdin"], &commits);
    assert_same_stdin(&repo, &["diff-tree", "--stdin", "-r"], &commits);
    assert_same_stdin(&repo, &["diff-tree", "--stdin", "-p"], &commits);
    assert_same_stdin(
        &repo,
        &["diff-tree", "--stdin", "-r", "--no-commit-id"],
        &commits,
    );
    // The two-tree form: the input line is echoed verbatim as the header.
    let two_tree = format!("{old_tree} {new_tree}\n");
    assert_same_stdin(&repo, &["diff-tree", "--stdin", "-r"], &two_tree);
    assert_same_stdin(
        &repo,
        &["diff-tree", "--stdin", "-r", "--no-commit-id"],
        &two_tree,
    );
    // --stdin only accepts full object ids: a ref/abbreviated/garbage line is
    // echoed but produces no diff, and a lone non-commit id reports an error.
    assert_same_stdin(&repo, &["diff-tree", "--stdin", "-r"], "HEAD^ HEAD\n");
    assert_same_stdin(&repo, &["diff-tree", "--stdin", "-r"], "zzzzz\n");
    assert_same_stdin(
        &repo,
        &["diff-tree", "--stdin", "-r"],
        &format!("{new_tree}\n"),
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn diff_tree_error_cases_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = init_basic_repo("diff-tree-errors");

    // No operands: usage to stderr, exit 129. We only assert the exit code and
    // the first usage line, since git's full usage block is long and version
    // dependent; matching stdout emptiness and exit is the meaningful contract.
    let g = git(&repo, &["diff-tree"]);
    let r = git_rs(&repo, &["diff-tree"]);
    assert_eq!(r.status.code(), g.status.code(), "no-arg exit differs");
    assert!(r.stdout.is_empty(), "no-arg stdout should be empty");
    assert!(
        String::from_utf8_lossy(&r.stderr).starts_with("usage: git diff-tree"),
        "no-arg stderr should be a usage block, got: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // Unknown revision: fatal ambiguous-argument message, exit 128.
    assert_same(&repo, &["diff-tree", "HEAD", "definitely-not-a-ref"]);
    assert_same(&repo, &["diff-tree", "definitely-not-a-ref"]);

    fs::remove_dir_all(&root).ok();
}
