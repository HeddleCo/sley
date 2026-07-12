//! Differential interop tests for `git show` vs the system `git` binary.
//!
//! Each test builds a throwaway repository with the system `git` (using a fixed
//! identity and timestamp so commit/tag oids and the rendered `Date:` lines are
//! deterministic), then asserts that `sley show ...` produces byte-identical
//! stdout, stderr, and exit code to `git show ...`. The whole suite is gated on
//! `git --version` succeeding, so it is a no-op where git is unavailable.

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

#[test]
fn show_uses_diff_interhunk_context_config() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("show-interhunk-context");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    git_ok(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("file"), b"A\n1\n2\n3\n4\n5\n6\n7\nB\n").expect("write base");
    git_ok(&repo, &["add", "file"]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("file"), b"X\n1\n2\n3\n4\n5\n6\n7\nY\n").expect("write change");
    git_ok(&repo, &["commit", "-qam", "change"]);
    git_ok(&repo, &["config", "diff.interHunkContext", "1"]);

    assert_same(&repo, &["show", "HEAD"]);
    fs::remove_dir_all(&root).ok();
}

/// Run a program with the fixed author/committer identity and date the task
/// mandates, so object ids and dates are reproducible across both binaries.
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
        // Force non-color, non-paged output regardless of the caller's config so
        // the comparison is stable.
        .env("GIT_PAGER", "cat")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sley(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::sley_bin!(), cwd, args)
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Trim a trailing `\n` so we can pass `git rev-parse` output as an argument.
fn rev_parse(cwd: &Path, spec: &str) -> String {
    let out = git(cwd, &["rev-parse", spec]);
    assert!(
        out.status.success(),
        "git rev-parse {spec} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("rev-parse output is utf8")
        .trim()
        .to_string()
}

/// Assert `sley show <args>` matches `git show <args>` on stdout, stderr, and
/// exit code.
fn assert_same(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = sley(cwd, args);
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

/// Build a small linear-then-merge history and return the repo path.
///
/// Layout (on the default branch after the merge):
/// - root commit: `a.txt` (multi-line message with a body)
/// - second commit: modifies `a.txt`, adds `b.txt`
/// - adds a subdirectory `sub/`
/// - an annotated tag `v1` and a lightweight tag `light` on the tip
/// - a `--no-ff` merge of a side branch
fn build_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("test operation should succeed"),
        ],
    );

    fs::write(repo.join("a.txt"), "hello\nworld\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "a.txt"]);
    git_ok(&repo, &["commit", "-qm", "first commit\n\nbody line"]);

    fs::write(repo.join("a.txt"), "hello\nthere\n").expect("test operation should succeed");
    fs::write(repo.join("b.txt"), "new file\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "second"]);

    fs::create_dir_all(repo.join("sub")).expect("test operation should succeed");
    fs::write(repo.join("sub").join("nested.txt"), "deep\n")
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "add subdir"]);

    git_ok(&repo, &["tag", "-a", "v1", "-m", "release one"]);
    git_ok(&repo, &["tag", "light"]);

    // Side branch + no-ff merge so we exercise a merge commit (Merge: line, no
    // patch by default).
    git_ok(&repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("feat.txt"), "feature\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "feature work"]);
    git_ok(&repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("c.txt"), "mainline\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "mainline work"]);
    git_ok(
        &repo,
        &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"],
    );

    repo
}

#[test]
fn show_commit_default_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-commit-default");

    // Default (medium header + patch), the root commit (empty-tree diff), and a
    // commit reached by relative spec.
    assert_same(&repo, &["show"]);
    assert_same(&repo, &["show", "HEAD~3"]);
    let root = rev_parse(&repo, "HEAD~3^{}");
    // The first commit on main is the root; show it by oid as well.
    let first = rev_parse(&repo, "main~4");
    assert_same(&repo, &["show", first.as_str()]);
    assert_same(&repo, &["show", root.as_str()]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_commit_formats_match_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-commit-formats");
    let head = rev_parse(&repo, "HEAD~1");

    for args in [
        vec!["show", "-s", head.as_str()],
        vec!["show", "--no-patch", head.as_str()],
        vec!["show", "--oneline", "-s", head.as_str()],
        vec!["show", "--abbrev-commit", "-s", head.as_str()],
        vec![
            "show",
            "--no-abbrev",
            "--abbrev-commit",
            "-s",
            head.as_str(),
        ],
        vec!["show", "--pretty=oneline", "-s", head.as_str()],
        vec!["show", "--format=%H %an <%ae>%n%s", "-s", head.as_str()],
        vec!["show", "--format=%h %s", head.as_str()],
        vec!["show", "--pretty=format:%h%n%s", head.as_str()],
        vec!["show", "--format=medium", "-s", head.as_str()],
        vec!["show", "--date=iso", "-s", head.as_str()],
        vec!["show", "--date=short", "-s", head.as_str()],
        // `git show` keeps the patch even for the oneline formats (no `-s`).
        vec!["show", "--oneline", head.as_str()],
        vec!["show", "--pretty=oneline", head.as_str()],
        vec!["show", "--oneline", "--stat", head.as_str()],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_commit_stat_modes_match_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-commit-stat");
    let head = rev_parse(&repo, "HEAD~1");

    for args in [
        vec!["show", "--stat", head.as_str()],
        vec!["show", "--shortstat", head.as_str()],
        vec!["show", "--numstat", head.as_str()],
        vec!["show", "--summary", head.as_str()],
        vec!["show", "--name-only", head.as_str()],
        vec!["show", "--name-status", head.as_str()],
        vec!["show", "-s", "--stat", head.as_str()],
        vec!["show", "--stat", "-s", head.as_str()],
        vec!["show", "--full-index", head.as_str()],
        vec!["show", "--abbrev=12", head.as_str()],
        vec!["show", "--pretty=format:%h", "--stat", head.as_str()],
        vec!["show", "--pretty=format:%h", "--name-only", head.as_str()],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_merge_commit_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-merge");

    // The tip is a merge commit. `git show` defaults a merge to the dense-combined
    // diff (`diff --cc`), so the patch/raw/name listings render the combined diff
    // against all parents, while the --stat family renders the first-parent diff
    // and `-s`/`--no-patch` suppresses the body. The exact trailing gap differs
    // per pretty format, so cover the spread.
    for args in [
        vec!["show", "HEAD"],
        vec!["show", "-s", "HEAD"],
        vec!["show", "-p", "HEAD"],
        vec!["show", "--name-only", "HEAD"],
        vec!["show", "--name-status", "HEAD"],
        vec!["show", "--oneline", "HEAD"],
        vec!["show", "--oneline", "-s", "HEAD"],
        vec!["show", "--stat", "HEAD"],
        vec!["show", "--shortstat", "HEAD"],
        vec!["show", "--numstat", "HEAD"],
        vec!["show", "--summary", "HEAD"],
        vec!["show", "--format=%h", "HEAD"],
        vec!["show", "--pretty=format:%h", "HEAD"],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_tag_tree_blob_match_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-tag-tree-blob");

    let tree_oid = rev_parse(&repo, "v1^{tree}");
    let tag_oid = rev_parse(&repo, "v1");

    for args in [
        // Annotated tag: tag block then the recursed commit + patch.
        vec!["show", "v1"],
        vec!["show", "v1", "-s"],
        vec!["show", tag_oid.as_str()],
        // Lightweight tag resolves straight to the commit.
        vec!["show", "light", "-s"],
        // Trees: header echoes the literal argument; entries are not recursive.
        vec!["show", "v1^{tree}"],
        vec!["show", tree_oid.as_str()],
        // Blobs: raw content, and the `<rev>:<path>` form.
        vec!["show", "HEAD~1:a.txt"],
        vec!["show", "HEAD~1:sub/nested.txt"],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_multiple_objects_match_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-multiple");
    let a = rev_parse(&repo, "main~1");
    let b = rev_parse(&repo, "main~2");

    for args in [
        // Several commits in one invocation (inter-entry separators).
        vec!["show", "-s", a.as_str(), b.as_str()],
        vec!["show", a.as_str(), b.as_str()],
        // Mixed object kinds: blobs do not get a leading separator, commits do.
        vec!["show", "-s", "main~1:a.txt", a.as_str(), b.as_str()],
        vec!["show", "-s", a.as_str(), "main~1:a.txt", b.as_str()],
        // Custom format across multiple commits (separator vs terminator).
        vec!["show", "--pretty=format:%h %s", a.as_str(), b.as_str()],
        vec!["show", "--format=%h %s", "-s", a.as_str(), b.as_str()],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn show_unknown_revision_matches_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-unknown-rev");

    // Unknown revision: identical fatal stderr and exit 128.
    assert_same(&repo, &["show", "no-such-rev"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

/// Ref decorations: off by default, on with `--decorate`/`--decorate=full`, and
/// auto-enabled for a custom format that references `%d`/`%D`.
#[test]
fn show_decorations_match_git() {
    if !git_available() {
        return;
    }
    let repo = build_repo("show-decorate");

    for args in [
        vec!["show", "-s", "main~1"],
        vec!["show", "--decorate", "-s", "main"],
        vec!["show", "--decorate=full", "-s", "main"],
        vec!["show", "--no-decorate", "-s", "main"],
        vec!["show", "--decorate", "--oneline", "-s", "main"],
        // `%d`/`%D` auto-enable decorations even without `--decorate`.
        vec!["show", "--format=[%d]", "-s", "main"],
        vec!["show", "--format=[%D]", "-s", "main"],
        vec!["show", "--decorate=full", "--format=[%d]", "-s", "main"],
        // An invalid --decorate value: identical fatal stderr and exit 128.
        vec!["show", "--decorate=bogus", "-s", "main"],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

/// A dedicated linear repo exercising deletions, nested-tree `<rev>:<path>`, the
/// `%b`/`%B` body placeholders, and an empty (no-change) commit.
#[test]
fn show_deletions_nested_and_body_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("show-misc");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("test operation should succeed"),
        ],
    );

    std::fs::create_dir_all(repo.join("dir").join("sub")).expect("test operation should succeed");
    std::fs::write(repo.join("dir").join("sub").join("deep.txt"), "x\ny\n")
        .expect("test operation should succeed");
    std::fs::write(repo.join("keep.txt"), "k\n").expect("test operation should succeed");
    // A filename with a space exercises path handling in the tree listing and
    // patch headers.
    std::fs::write(repo.join("with space.txt"), "spaced\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(
        &repo,
        &["commit", "-qm", "init\n\na body paragraph\nsecond line"],
    );

    git_ok(&repo, &["rm", "-q", "keep.txt"]);
    git_ok(&repo, &["commit", "-qm", "remove keep"]);

    git_ok(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "empty change"],
    );

    let subtree = rev_parse(&repo, "HEAD~2:dir");

    for args in [
        // Deletion patch (`deleted file mode`, `+++ /dev/null`).
        vec!["show", "HEAD~1"],
        vec!["show", "HEAD~1", "--summary"],
        vec!["show", "HEAD~1", "--name-status"],
        // Empty commit: no diff, no trailing gap for the medium/tformat forms.
        vec!["show", "HEAD"],
        vec!["show", "HEAD", "--stat"],
        vec!["show", "HEAD", "--format=%h", "-s"],
        vec!["show", "HEAD", "--pretty=format:%h"],
        // Body placeholders.
        vec!["show", "HEAD~2", "--format=%b", "-s"],
        vec!["show", "HEAD~2", "--format=%B", "-s"],
        vec!["show", "HEAD~2", "--format=%s%n%b", "-s"],
        // Nested-tree and blob via `<rev>:<path>`, plus a sub-tree by oid.
        vec!["show", "HEAD~2:dir"],
        vec!["show", "HEAD~2:dir/sub"],
        vec!["show", "HEAD~2:dir/sub/deep.txt"],
        vec!["show", subtree.as_str()],
        // The root tree lists `dir/`, `keep.txt`, and `with space.txt`; the patch
        // header quotes the spaced path the same way git does.
        vec!["show", "HEAD~2^{tree}"],
        vec!["show", "HEAD~2:with space.txt"],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(&root).ok();
}
