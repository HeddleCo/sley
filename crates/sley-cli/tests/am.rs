//! Differential interop tests for `git am` vs the system `git`.
//!
//! Each case builds patches with the system `git format-patch`, then replays
//! them through both the system `git am` and `sley am` against byte-identical
//! target repositories and asserts the two agree on stdout, stderr, exit status,
//! the resulting commit graph (including commit OIDs — `git am` reproduces the
//! original commits, so a correct implementation is hash-identical), and the
//! worktree contents.
//!
//! Every case is gated on the system `git` being runnable so the suite is a
//! no-op where git is unavailable. All git invocations use a fixed identity and
//! date so commit OIDs are deterministic and comparable across the two binaries.

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

/// Run a program with the fixed identity/date used across the whole suite.
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
        // The am suite runs in parallel. Never inherit the developer/runner's
        // global config (maintenance, filters, autocrlf, hooks, and fsmonitor
        // settings can otherwise leak into one side of a parity fixture).
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", cwd.join(".xdg-config"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // The absolute oracle binary uses its compiled libexec path for Git
        // subcommands. Do not replace it with bin/, which hides git-submodule.
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn sley(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::sley_bin!(), cwd, args)
}

/// Run a git command and assert success, returning trimmed stdout. Used for
/// fixture setup (not part of the behaviour under test).
fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = git(cwd, args);
    assert!(
        out.status.success(),
        "setup `git {args:?}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write(dir: &Path, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap_or_else(|err| panic!("write {name}: {err}"));
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy fixture file");
        }
    }
}

/// Initialise a repo on `main` with one commit creating `file.txt`.
fn init_repo(dir: &Path, initial: &str) {
    fs::create_dir_all(dir).expect("create repo dir");
    git_ok(dir, &["init", "-q", "-b", "main"]);
    write(dir, "file.txt", initial);
    git_ok(dir, &["add", "file.txt"]);
    git_ok(dir, &["commit", "-q", "-m", "initial"]);
}

/// Compare the two repos' commit graphs (OID + identities + dates + subject) and
/// the contents of `file.txt`.
fn assert_repos_equal(git_dir: &Path, rs_dir: &Path) {
    let fmt = "--format=%H|%an|%ae|%ad|%cn|%ce|%cd|%s";
    let g_log = git_ok(git_dir, &["log", fmt, "--date=raw"]);
    let r_log = git_ok(rs_dir, &["log", fmt, "--date=raw"]);
    assert_eq!(r_log, g_log, "commit graph (with OIDs) differs after am");

    let g_file = fs::read(git_dir.join("file.txt")).ok();
    let r_file = fs::read(rs_dir.join("file.txt")).ok();
    assert_eq!(
        r_file.as_deref().map(String::from_utf8_lossy),
        g_file.as_deref().map(String::from_utf8_lossy),
        "file.txt differs after am"
    );
}

/// Assert two outputs agree on stdout, stderr, and exit code for `label`.
fn assert_outputs_equal(label: &str, g: &Output, r: &Output) {
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "{label}: stdout differs\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "{label}: stderr differs",
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "{label}: exit code differs"
    );
}

/// Collect the `*.patch` files in `dir`, sorted, as owned path strings.
fn patch_paths(dir: &Path) -> Vec<String> {
    let mut paths: Vec<String> = fs::read_dir(dir)
        .expect("read patch dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "patch").unwrap_or(false))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no patches produced in {dir:?}");
    paths
}

/// A clean linear series produced by `format-patch` replays to byte-identical
/// commits (same OIDs) through both binaries, with matching `Applying:` output.
#[test]
fn am_clean_series_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-clean");
    let source = root.join("source");

    // Build a source history: two commits with distinct authors/dates so the
    // mbox carries non-trivial From/Date headers, plus a new-file commit.
    init_repo(&source, "line1\nline2\nline3\n");
    write(&source, "file.txt", "line1\nCHANGED\nline3\n");
    git_ok(&source, &["add", "file.txt"]);
    run_env(
        sley_testkit::oracle_git(),
        &source,
        &[
            "commit",
            "-q",
            "-m",
            "second subject\n\nBody line one.\nBody line two.",
        ],
    );
    write(&source, "added.txt", "brand new file\n");
    git_ok(&source, &["add", "added.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "add a file"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-2",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    init_repo(&git_target, "line1\nline2\nline3\n");
    init_repo(&rs_target, "line1\nline2\nline3\n");

    let mut g_args = vec!["am"];
    g_args.extend_from_slice(&patch_args);
    let g = git(&git_target, &g_args);
    let r = sley(&rs_target, &g_args);

    assert_outputs_equal("am clean series", &g, &r);
    assert_repos_equal(&git_target, &rs_target);
    // Both binaries clean up the series state on success.
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn am_in_cone_preserves_sparse_index_layout() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-sparse-layout");
    let source = root.join("source");
    fs::create_dir_all(source.join("deep")).expect("create source deep");
    fs::create_dir_all(source.join("other")).expect("create source other");
    git_ok(&source, &["init", "-q", "-b", "main"]);
    write(&source.join("deep"), "a", "base\n");
    write(&source.join("other"), "a", "other\n");
    git_ok(&source, &["add", "."]);
    git_ok(&source, &["commit", "-qm", "base"]);
    write(&source.join("deep"), "a", "changed\n");
    git_ok(&source, &["commit", "-qam", "changed"]);
    let patch = root.join("change.patch");
    let formatted = git(&source, &["format-patch", "-1", "--stdout"]);
    assert!(formatted.status.success());
    fs::write(&patch, formatted.stdout).expect("write patch");

    git_ok(&source, &["reset", "--hard", "HEAD^"]);
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    copy_dir_all(&source, &git_target);
    copy_dir_all(&source, &rs_target);
    for repo in [&git_target, &rs_target] {
        // The recursive fixture copy preserves the source index's stat cache,
        // but the target files have different inode metadata. Refresh first so
        // sparse-checkout can safely collapse the out-of-cone directory.
        git_ok(repo, &["update-index", "--refresh"]);
        git_ok(repo, &["config", "advice.sparseIndexExpanded", "false"]);
        git_ok(repo, &["sparse-checkout", "set", "--sparse-index", "deep"]);
    }
    let patch_arg = patch.to_string_lossy();
    let expected = git(&git_target, &["am", patch_arg.as_ref()]);
    let actual = sley(&rs_target, &["am", patch_arg.as_ref()]);
    assert_outputs_equal("am sparse layout", &expected, &actual);
    let expected_layout = git(&git_target, &["ls-files", "--sparse"]);
    let actual_layout = git(&rs_target, &["ls-files", "--sparse"]);
    assert_eq!(actual_layout.stdout, expected_layout.stdout);
    assert!(String::from_utf8_lossy(&actual_layout.stdout).contains("other/\n"));

    fs::remove_dir_all(&root).ok();
}

/// A patch whose context no longer matches stops with a conflict: stdout,
/// stderr (error + hint block), and exit status all match, and the partially
/// applied state leaves both repos at the same HEAD.
#[test]
fn am_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-conflict");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "patch commit"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    // Targets diverge from the patch's base on the same line, so it cannot apply.
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nLOCAL\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "local change"]);
    }

    let mut args = vec!["am"];
    args.extend_from_slice(&patch_args);
    let g = git(&git_target, &args);
    let r = sley(&rs_target, &args);

    assert_outputs_equal("am conflict", &g, &r);
    // HEAD must be unchanged (the failing patch was not committed) and equal.
    assert_eq!(
        git_ok(&rs_target, &["rev-parse", "HEAD"]),
        git_ok(&git_target, &["rev-parse", "HEAD"]),
        "HEAD differs after conflicting am"
    );
    assert!(rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

/// `git am --abort` after a conflict restores the original branch and removes
/// the series state, identically across both binaries.
#[test]
fn am_abort_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-abort");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "patch commit"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nLOCAL\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "local change"]);
    }

    let mut start = vec!["am"];
    start.extend_from_slice(&patch_args);
    // Drive into the conflict state first (output already covered elsewhere).
    let _ = git(&git_target, &start);
    let _ = sley(&rs_target, &start);

    let g_abort = git(&git_target, &["am", "--abort"]);
    let r_abort = sley(&rs_target, &["am", "--abort"]);
    assert_outputs_equal("am --abort", &g_abort, &r_abort);
    assert_repos_equal(&git_target, &rs_target);
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

/// Resolving a conflict and running `git am --continue` finalises the patch with
/// the preserved author/message and resumes the series; both binaries land the
/// same commit (same OID) and clean up.
#[test]
fn am_continue_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-continue");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "patch subject line"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nLOCAL\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "local change"]);
    }

    let mut start = vec!["am"];
    start.extend_from_slice(&patch_args);
    let _ = git(&git_target, &start);
    let _ = sley(&rs_target, &start);

    // Resolve identically in both worktrees, then continue.
    for target in [&git_target, &rs_target] {
        write(target, "file.txt", "A\nRESOLVED\nC\n");
        git_ok(target, &["add", "file.txt"]);
    }
    let g_cont = git(&git_target, &["am", "--continue"]);
    let r_cont = sley(&rs_target, &["am", "--continue"]);

    assert_outputs_equal("am --continue", &g_cont, &r_cont);
    assert_repos_equal(&git_target, &rs_target);
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

/// `git am --skip` discards the current (conflicting) patch and resumes; with a
/// single failing patch the series ends with HEAD unchanged in both binaries.
#[test]
fn am_skip_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-skip");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "patch commit"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nLOCAL\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "local change"]);
    }

    let mut start = vec!["am"];
    start.extend_from_slice(&patch_args);
    let _ = git(&git_target, &start);
    let _ = sley(&rs_target, &start);

    let g_skip = git(&git_target, &["am", "--skip"]);
    let r_skip = sley(&rs_target, &["am", "--skip"]);
    assert_outputs_equal("am --skip", &g_skip, &r_skip);
    assert_repos_equal(&git_target, &rs_target);
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

/// A failed three-way apply can leave a directory where HEAD has a file. The
/// directory's children are apply-owned cleanup paths, so `am --skip` must
/// remove them and restore HEAD instead of misclassifying them as unrelated
/// untracked data.
#[test]
fn am_skip_cleans_directory_file_conflict_paths() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-skip-directory-file");
    let source = root.join("source");
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    let numbers = (1..=10)
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    for repo in [&source, &git_target, &rs_target] {
        fs::create_dir_all(repo).expect("create D/F repo");
        git_ok(repo, &["init", "-q", "-b", "main"]);
        write(repo, "numbers", &numbers);
        git_ok(repo, &["add", "numbers"]);
        git_ok(repo, &["commit", "-q", "-m", "initial"]);
    }

    fs::create_dir(source.join("foo")).expect("create source foo directory");
    write(&source.join("foo"), "bar", "content\n");
    fs::write(source.join("numbers"), format!("{numbers}11\n")).expect("edit source numbers");
    git_ok(&source, &["add", "foo", "numbers"]);
    git_ok(&source, &["commit", "-q", "-m", "directory and edit"]);

    for target in [&git_target, &rs_target] {
        write(target, "foo", "content\n");
        fs::write(target.join("numbers"), format!("{numbers}eleven\n"))
            .expect("edit target numbers");
        git_ok(target, &["add", "foo", "numbers"]);
        git_ok(target, &["commit", "-q", "-m", "file and edit"]);
    }

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("create patch directory");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patch = patch_paths(&patches_dir).remove(0);
    assert!(!git(&git_target, &["am", "-3", &patch]).status.success());
    assert!(!sley(&rs_target, &["am", "-3", &patch]).status.success());

    let expected = git(&git_target, &["am", "--skip"]);
    let actual = sley(&rs_target, &["am", "--skip"]);
    assert_outputs_equal("am --skip after D/F conflict", &expected, &actual);
    assert_repos_equal(&git_target, &rs_target);
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());
    assert!(git_ok(&rs_target, &["ls-files", "-u"]).is_empty());

    fs::remove_dir_all(&root).ok();
}

/// A `-3` fallback that succeeds (non-overlapping edits) reconstructs the base
/// from the patch's blobs and 3-way merges; both binaries produce the same merge
/// output, the same commit OID, and the same worktree.
#[test]
fn am_three_way_success_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-3way");
    let source = root.join("source");
    init_repo(&source, "L1\nL2\nL3\nL4\nL5\n");
    write(&source, "file.txt", "L1\nL2\nMID\nL4\nL5\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "change the middle"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    // Targets edit a *different* line than the patch, so straight application
    // fails but the 3-way merge succeeds.
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "L1\nL2\nL3\nL4\nL5\n");
        write(target, "file.txt", "TOP\nL2\nL3\nL4\nL5\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "change the top"]);
    }

    let mut args = vec!["am", "-3"];
    args.extend_from_slice(&patch_args);
    let g = git(&git_target, &args);
    let r = sley(&rs_target, &args);

    assert_outputs_equal("am -3 success", &g, &r);
    assert_repos_equal(&git_target, &rs_target);
    assert!(!rs_target.join(".git/rebase-apply").exists());

    fs::remove_dir_all(&root).ok();
}

/// A `-3` fallback that conflicts (overlapping edits) leaves identical conflict
/// markers and index state, with matching stdout/stderr/exit.
#[test]
fn am_three_way_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-3way-conflict");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nTHEIRS\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "their middle change"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nOURS\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "our middle change"]);
    }

    let mut args = vec!["am", "-3"];
    args.extend_from_slice(&patch_args);
    let g = git(&git_target, &args);
    let r = sley(&rs_target, &args);

    assert_outputs_equal("am -3 conflict", &g, &r);
    // The conflicted worktree file (with markers) and the index state match.
    assert_eq!(
        fs::read_to_string(rs_target.join("file.txt")).expect("rs file"),
        fs::read_to_string(git_target.join("file.txt")).expect("git file"),
        "conflict markers differ for am -3"
    );
    assert_eq!(
        git_ok(&rs_target, &["status", "--short"]),
        git_ok(&git_target, &["status", "--short"]),
        "status differs for am -3 conflict"
    );

    fs::remove_dir_all(&root).ok();
}

/// The resume sub-operations with no series in progress all fail the same way.
#[test]
fn am_resume_without_progress_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-no-progress");
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    init_repo(&git_target, "x\n");
    init_repo(&rs_target, "x\n");

    for sub in ["--abort", "--continue", "--skip", "--resolved", "--quit"] {
        let g = git(&git_target, &["am", sub]);
        let r = sley(&rs_target, &["am", sub]);
        assert_outputs_equal(&format!("am {sub} (no progress)"), &g, &r);
    }

    fs::remove_dir_all(&root).ok();
}

/// Empty input is a silent no-op success; non-empty input with no patch reports
/// "Patch is empty." with git's hint block and exits 128. Starting a fresh `am`
/// while a series is in progress is rejected with the same message.
#[test]
fn am_empty_and_in_progress_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-empty");
    let git_target = root.join("git");
    let rs_target = root.join("rs");
    init_repo(&git_target, "x\n");
    init_repo(&rs_target, "x\n");

    // Empty mbox file -> exit 0, no output.
    let empty = root.join("empty.mbox");
    fs::write(&empty, b"").expect("write empty mbox");
    let g = git(&git_target, &["am", empty.to_string_lossy().as_ref()]);
    let r = sley(&rs_target, &["am", empty.to_string_lossy().as_ref()]);
    assert_outputs_equal("am empty mbox", &g, &r);

    // Non-empty, non-patch input -> "Patch is empty." + hints, exit 128.
    let garbage = root.join("garbage.mbox");
    fs::write(&garbage, b"this is not a patch\n").expect("write garbage");
    let g = git(&git_target, &["am", garbage.to_string_lossy().as_ref()]);
    let r = sley(&rs_target, &["am", garbage.to_string_lossy().as_ref()]);
    assert_outputs_equal("am garbage mbox", &g, &r);

    // With a series now in progress (from the garbage case), starting another am
    // is rejected with the relative ".git/rebase-apply" path in the message.
    let g = git(&git_target, &["am", garbage.to_string_lossy().as_ref()]);
    let r = sley(&rs_target, &["am", garbage.to_string_lossy().as_ref()]);
    assert_outputs_equal("am while in progress", &g, &r);

    fs::remove_dir_all(&root).ok();
}

/// format-patch emits a file→gitlink type-change as delete + new-gitlink. `am`
/// (and `am --3way`, which prefers the straight apply when it succeeds) must
/// accept that split, stage the gitlink, and leave an empty submodule directory
/// (t4255 "replace tracked file with submodule creates empty directory").
#[test]
fn am_file_to_gitlink_type_change_creates_empty_dir_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-file-to-gitlink");
    let sub = root.join("sub");
    fs::create_dir_all(&sub).expect("sub dir");
    git_ok(&sub, &["init", "-q", "-b", "main"]);
    write(&sub, "file1", "x\n");
    git_ok(&sub, &["add", "file1"]);
    git_ok(&sub, &["commit", "-q", "-m", "sub base"]);

    let source = root.join("source");
    fs::create_dir_all(&source).expect("source dir");
    git_ok(&source, &["init", "-q", "-b", "main"]);
    git_ok(
        &source,
        &[
            "-c",
            "protocol.file.allow=always",
            "config",
            "protocol.file.allow",
            "always",
        ],
    );
    write(&source, "file2", "y\n");
    git_ok(&source, &["add", "file2"]);
    git_ok(&source, &["commit", "-q", "-m", "base"]);
    git_ok(
        &source,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.to_string_lossy().as_ref(),
            "sub1",
        ],
    );
    git_ok(&source, &["commit", "-q", "-m", "add sub1"]);
    git_ok(&source, &["rm", "sub1"]);
    write(&source, "sub1", "content\n");
    git_ok(&source, &["add", "sub1"]);
    git_ok(&source, &["commit", "-q", "-m", "file"]);
    let file_oid = git_ok(&source, &["rev-parse", "HEAD"]);
    git_ok(&source, &["revert", "--no-edit", "HEAD"]);
    let sub_oid = git_ok(&source, &["rev-parse", "HEAD"]);

    let patches = root.join("patches");
    fs::create_dir_all(&patches).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            &sub_oid,
            "-o",
            patches.to_string_lossy().as_ref(),
        ],
    );
    let patch = patch_paths(&patches).remove(0);

    for (label, am_args) in [
        ("am", vec!["am", patch.as_str()]),
        ("am --3way", vec!["am", "--3way", patch.as_str()]),
    ] {
        let git_target = root.join(format!("git-{label}"));
        let rs_target = root.join(format!("rs-{label}"));
        for target in [&git_target, &rs_target] {
            copy_dir_all(&source, target);
            // Detach to the file state so the type-change patch is the one to apply.
            git_ok(target, &["checkout", "-q", "-f", &file_oid]);
            // Drop any residual submodule checkout left by the copy.
            let _ = fs::remove_dir_all(target.join("sub1"));
            write(target, "sub1", "content\n");
            git_ok(target, &["reset", "-q", "--hard", &file_oid]);
        }

        let g = git(&git_target, &am_args);
        let r = sley(&rs_target, &am_args);
        assert!(
            g.status.success(),
            "{label}: oracle am failed: {}",
            String::from_utf8_lossy(&g.stderr)
        );
        assert!(
            r.status.success(),
            "{label}: sley am failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            git_ok(&rs_target, &["rev-parse", "HEAD"]),
            git_ok(&git_target, &["rev-parse", "HEAD"]),
            "{label}: HEAD OID differs after am"
        );
        assert_eq!(
            git_ok(&rs_target, &["ls-files", "-s", "sub1"]),
            git_ok(&git_target, &["ls-files", "-s", "sub1"]),
            "{label}: gitlink index entry differs"
        );
        assert!(
            rs_target.join("sub1").is_dir(),
            "{label}: sley must leave empty submodule directory"
        );
        assert!(
            fs::read_dir(rs_target.join("sub1"))
                .expect("read sub1")
                .next()
                .is_none(),
            "{label}: sley submodule directory must be empty"
        );
        assert_eq!(
            fs::read_dir(rs_target.join("sub1"))
                .expect("sley sub1")
                .count(),
            fs::read_dir(git_target.join("sub1"))
                .expect("git sub1")
                .count(),
            "{label}: empty-dir content differs"
        );
    }

    fs::remove_dir_all(&root).ok();
}

/// `am --reject` keeps every hunk that applies, writes the failed fragment to
/// `<path>.rej`, leaves the index at HEAD, and persists the option for retry.
#[test]
fn am_reject_partially_applies_and_writes_reject_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-reject");
    let source = root.join("source");
    init_repo(&source, "0\n2\n3\n4\n5\n6\n7\n");
    write(&source, "file.txt", "One\n2\n3\n4\n5\nSix\n7\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "two separated changes"]);

    let patches = root.join("patches");
    fs::create_dir_all(&patches).expect("create patch directory");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patch = patch_paths(&patches).remove(0);

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "1\n2\n3\n4\n5\n6\n7\n");
        filetime::set_file_mtime(
            target.join("file.txt"),
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .expect("age rejected-patch fixture");
    }

    // Refresh the fixture's cached stat data before applying, as Git's own
    // tests do for racy-stat hygiene. The aged mtime keeps the subsequent
    // byte-identical rewrite outside the index's racy-clean timestamp window.
    let _ = git(&rs_target, &["update-index", "-q", "--refresh"]);
    let _ = git(&git_target, &["update-index", "-q", "--refresh"]);
    let g = git(&git_target, &["am", "--reject", &patch]);
    let r = sley(&rs_target, &["am", "--reject", &patch]);
    assert!(
        !g.status.success(),
        "Git fixture unexpectedly applied cleanly"
    );
    assert!(!r.status.success(), "Sley unexpectedly applied cleanly");

    assert_eq!(
        fs::read(rs_target.join("file.txt")).expect("Sley partial file"),
        fs::read(git_target.join("file.txt")).expect("Git partial file"),
        "partially-applied worktree bytes differ"
    );
    assert_eq!(
        fs::read(rs_target.join("file.txt.rej")).expect("Sley reject file"),
        fs::read(git_target.join("file.txt.rej")).expect("Git reject file"),
        "reject-file bytes differ"
    );
    let expected_dirty = "file.txt";
    assert_eq!(
        git_ok(&rs_target, &["diff-files", "--name-only"]),
        expected_dirty,
        "Sley must materialize the rejected patch result"
    );
    assert_eq!(
        git_ok(&git_target, &["diff-files", "--name-only"]),
        expected_dirty,
        "Git fixture must exercise byte-identical materialization"
    );
    assert_eq!(
        fs::read(rs_target.join(".git/rebase-apply/apply-opt")).expect("Sley apply-opt"),
        fs::read(git_target.join(".git/rebase-apply/apply-opt")).expect("Git apply-opt"),
        "persisted apply options differ"
    );

    fs::remove_dir_all(&root).ok();
}

/// `am --abort` after a NON-conflict stop must still rewind to the pre-series
/// HEAD. Regression: abort-safety was only refreshed at conflict stops, so a
/// series whose first patch applies clean and whose second patch stops without
/// a conflict (empty patch under `--empty=stop`) compared against the stale
/// pre-series tip, printed the false "You seem to have moved HEAD" warning,
/// deleted the state, and left patch 1 applied.
#[test]
fn am_abort_after_non_conflict_stop_rewinds_to_pre_series_head() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-abort-nonconflict");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");

    // Patch 1 applies cleanly onto the base; patch 2 is an empty commit, so
    // `--empty=stop` halts the series after patch 1 has already landed.
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "real patch"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    // Second patch: an empty (no-diff) commit so format-patch emits a patch
    // with headers but no diff body.
    git_ok(
        &source,
        &["commit", "-q", "--allow-empty", "-m", "empty patch"],
    );
    git_ok(
        &source,
        &[
            "format-patch",
            "-2",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    assert_eq!(patches.len(), 2, "expected a two-patch series");
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    init_repo(&git_target, "A\nB\nC\n");
    init_repo(&rs_target, "A\nB\nC\n");
    let base = git_ok(&git_target, &["rev-parse", "HEAD"]);

    let mut start = vec!["am", "--empty=stop"];
    start.extend_from_slice(&patch_args);
    let g = git(&git_target, &start);
    let r = sley(&rs_target, &start);
    assert_outputs_equal("am --empty=stop", &g, &r);
    assert!(!g.status.success(), "oracle must stop on the empty patch");
    assert!(git_target.join(".git/rebase-apply").exists());
    assert!(rs_target.join(".git/rebase-apply").exists());
    // Both sides applied patch 1 before stopping.
    assert_ne!(
        git_ok(&rs_target, &["rev-parse", "HEAD"]),
        base,
        "fixture requires patch 1 to have committed before the stop"
    );

    let g_abort = git(&git_target, &["am", "--abort"]);
    let r_abort = sley(&rs_target, &["am", "--abort"]);
    assert_outputs_equal("am --abort after non-conflict stop", &g_abort, &r_abort);

    // The abort must rewind to the PRE-SERIES head on both sides — no false
    // "moved HEAD" warning, and the state directory removed.
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());
    assert_repos_equal(&git_target, &rs_target);
    assert_eq!(git_ok(&rs_target, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read(rs_target.join("file.txt")).expect("worktree restored"),
        b"A\nB\nC\n".to_vec(),
        "patch 1 must not survive the abort"
    );
    assert_eq!(
        git_ok(&rs_target, &["status", "--porcelain"]),
        "",
        "index/worktree must be clean after the rewind"
    );

    fs::remove_dir_all(&root).ok();
}

/// `am --quit` must clear rerere's in-progress state like --abort/--skip do.
/// Regression: quit skipped am_rerere_clear, leaving MERGE_RR behind to seed
/// phantom conflicts in later operations.
#[test]
fn am_quit_clears_rerere_state() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("am-quit-rerere");
    let source = root.join("source");
    init_repo(&source, "A\nB\nC\n");
    write(&source, "file.txt", "A\nPATCHED\nC\n");
    git_ok(&source, &["add", "file.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "patch commit"]);

    let patches_dir = root.join("patches");
    fs::create_dir_all(&patches_dir).expect("patches dir");
    git_ok(
        &source,
        &[
            "format-patch",
            "-1",
            "-o",
            patches_dir.to_string_lossy().as_ref(),
            "HEAD",
        ],
    );
    let patches = patch_paths(&patches_dir);
    let patch_args: Vec<&str> = patches.iter().map(String::as_str).collect();

    let git_target = root.join("git");
    let rs_target = root.join("rs");
    for target in [&git_target, &rs_target] {
        init_repo(target, "A\nB\nC\n");
        write(target, "file.txt", "A\nLOCAL\nC\n");
        git_ok(target, &["add", "file.txt"]);
        git_ok(target, &["commit", "-q", "-m", "local change"]);
        git_ok(target, &["config", "rerere.enabled", "true"]);
    }

    let mut start = vec!["am", "-3"];
    start.extend_from_slice(&patch_args);
    let _ = git(&git_target, &start);
    let _ = sley(&rs_target, &start);

    // The conflicted three-way merge records its preimage in MERGE_RR on both
    // sides (plain apply-path conflicts never touch rerere).
    assert!(
        git_target.join(".git/MERGE_RR").exists(),
        "oracle fixture must exercise rerere recording"
    );
    assert!(
        rs_target.join(".git/MERGE_RR").exists(),
        "sley fixture must exercise rerere recording"
    );

    let g_quit = git(&git_target, &["am", "--quit"]);
    let r_quit = sley(&rs_target, &["am", "--quit"]);
    assert_outputs_equal("am --quit with rerere", &g_quit, &r_quit);
    assert!(!git_target.join(".git/rebase-apply").exists());
    assert!(!rs_target.join(".git/rebase-apply").exists());
    assert!(
        !rs_target.join(".git/MERGE_RR").exists(),
        "am --quit must clear MERGE_RR (stale seeds cause phantom conflicts)"
    );

    fs::remove_dir_all(&root).ok();
}
