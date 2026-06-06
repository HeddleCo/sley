//! Differential interop tests for `git format-patch` vs the system `git` binary.
//!
//! Each test builds a throwaway repository with the system `git` (using a fixed
//! identity and author/committer date so commit oids and the rendered `Date:`
//! lines are deterministic), then asserts that `sley format-patch ...` produces
//! byte-identical output to `git format-patch ...` — both the `--stdout` stream
//! and, for the file-output mode, the generated `.patch` files plus the list of
//! file names printed to stdout.
//!
//! The suite is a no-op unless the system `git` is available *and* reports the
//! same version sley targets: the mbox `-- \n<version>` signature trailer
//! embeds the git version string, so byte-for-byte equality is only meaningful
//! against the matching upstream release.

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

/// The suite runs only when the system `git` is present and its version matches
/// the one sley targets, because the patch signature trailer embeds that
/// version string.
fn interop_enabled() -> bool {
    let Ok(git_version) = Command::new("git").arg("--version").output() else {
        return false;
    };
    if !git_version.status.success() {
        return false;
    }
    let git_version = String::from_utf8_lossy(&git_version.stdout);
    // `git --version` prints "git version X.Y.Z"; sley mirrors that.
    let Ok(rs_version) = Command::new(env!("CARGO_BIN_EXE_sley"))
        .arg("version")
        .output()
    else {
        return false;
    };
    if !rs_version.status.success() {
        return false;
    }
    let rs_version = String::from_utf8_lossy(&rs_version.stdout);
    git_version.trim() == rs_version.trim()
}

/// Assert `sley format-patch <args>` matches `git format-patch <args>` on
/// stdout, stderr, and exit code.
fn assert_same_stdout(cwd: &Path, args: &[&str]) {
    let g = git(cwd, args);
    let r = git_rs(cwd, args);
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

/// Sorted list of file names in a directory.
fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read output dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Run both binaries in file-output mode (`-o <dir>`) into separate directories
/// and assert the printed file list and every generated patch file are
/// byte-identical. `extra` carries the commit selection / option flags.
fn assert_same_files(repo: &Path, extra: &[&str]) {
    let git_dir = repo.join("git-out");
    let rs_dir = repo.join("rs-out");
    let _ = fs::remove_dir_all(&git_dir);
    let _ = fs::remove_dir_all(&rs_dir);

    let mut git_args = vec!["format-patch", "-o", "git-out"];
    git_args.extend_from_slice(extra);
    let mut rs_args = vec!["format-patch", "-o", "rs-out"];
    rs_args.extend_from_slice(extra);

    let g = git(repo, &git_args);
    let r = git_rs(repo, &rs_args);
    assert!(
        g.status.success(),
        "git {git_args:?} failed: {}",
        String::from_utf8_lossy(&g.stderr)
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit differs for files {extra:?}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // The printed paths differ only by the directory name we chose, so normalize
    // that before comparing the listing.
    let g_list = String::from_utf8_lossy(&g.stdout).replace("git-out/", "OUT/");
    let r_list = String::from_utf8_lossy(&r.stdout).replace("rs-out/", "OUT/");
    assert_eq!(r_list, g_list, "file listing differs for {extra:?}");

    let g_names = dir_entries(&git_dir);
    let r_names = dir_entries(&rs_dir);
    assert_eq!(
        r_names, g_names,
        "generated file names differ for {extra:?}"
    );

    for name in &g_names {
        let g_bytes = fs::read(git_dir.join(name)).expect("read git patch");
        let r_bytes = fs::read(rs_dir.join(name)).expect("read sley patch");
        assert_eq!(
            String::from_utf8_lossy(&r_bytes),
            String::from_utf8_lossy(&g_bytes),
            "file {name} differs for {extra:?}"
        );
    }
}

/// Build a linear history with a variety of changes the patch renderer must
/// handle: multi-line bodies, additions, modifications, deletions, renames,
/// mode changes, a no-trailing-newline file, a path containing a space, and a
/// commit with a long subject that the `Subject:` header folds.
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

    fs::write(repo.join("a.txt"), "alpha\nbeta\ngamma\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "a.txt"]);
    git_ok(&repo, &["commit", "-qm", "first commit"]);

    fs::write(repo.join("a.txt"), "alpha\nBETA\ngamma\ndelta\n")
        .expect("test operation should succeed");
    fs::write(repo.join("b.txt"), "new file\n").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(
        &repo,
        &[
            "commit",
            "-qm",
            "second commit\n\nThis is a longer body.\nWith multiple lines.",
        ],
    );

    fs::create_dir_all(repo.join("sub")).expect("test operation should succeed");
    fs::write(repo.join("sub").join("nested.txt"), "deep\n")
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "add nested directory"]);

    // Rename (with a small content tweak so detection still reports a rename).
    git_ok(&repo, &["mv", "b.txt", "renamed.txt"]);
    git_ok(&repo, &["commit", "-qm", "rename b to renamed"]);

    // Mode change to executable.
    let exec = repo.join("renamed.txt");
    let mut perms = fs::metadata(&exec)
        .expect("test operation should succeed")
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    fs::set_permissions(&exec, perms).expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "make renamed executable"]);

    // A file with no trailing newline.
    fs::write(repo.join("nonl.txt"), "no newline here").expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(
        &repo,
        &["commit", "-qm", "add file without trailing newline"],
    );

    // A path containing a space.
    fs::write(repo.join("with space.txt"), "spaced content\n")
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "add a path with a space"]);

    // Delete a file.
    git_ok(&repo, &["rm", "-q", "sub/nested.txt"]);
    git_ok(&repo, &["commit", "-qm", "remove nested file"]);

    // A commit whose subject is long enough to exercise header folding.
    fs::write(repo.join("a.txt"), "alpha\nBETA\ngamma\ndelta\nepsilon\n")
        .expect("test operation should succeed");
    git_ok(&repo, &["add", "-A"]);
    git_ok(
        &repo,
        &[
            "commit",
            "-qm",
            "this is a deliberately long commit subject line that should exceed the email wrap width and fold",
        ],
    );

    repo
}

#[test]
fn format_patch_stdout_matches_git() {
    if !interop_enabled() {
        return;
    }
    let repo = build_repo("format-patch-stdout");

    for args in [
        // Last-n selection, oldest-first ordering, default numbering.
        vec!["format-patch", "-1", "--stdout"],
        vec!["format-patch", "-2", "--stdout"],
        vec!["format-patch", "-3", "--stdout"],
        // The whole history including the root commit (empty-tree diff).
        vec!["format-patch", "-9", "--stdout"],
        // Body rendering and the diffstat for the second commit.
        vec!["format-patch", "-1", "HEAD~7", "--stdout"],
        // Rename, mode change, no-newline, space-in-path, deletion commits.
        vec!["format-patch", "-1", "HEAD~5", "--stdout"],
        vec!["format-patch", "-1", "HEAD~4", "--stdout"],
        vec!["format-patch", "-1", "HEAD~3", "--stdout"],
        vec!["format-patch", "-1", "HEAD~2", "--stdout"],
        vec!["format-patch", "-1", "HEAD~1", "--stdout"],
        // Long subject that folds across header lines.
        vec!["format-patch", "-1", "--stdout"],
    ] {
        assert_same_stdout(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn format_patch_ranges_match_git() {
    if !interop_enabled() {
        return;
    }
    let repo = build_repo("format-patch-ranges");

    for args in [
        // A bare committish means `<rev>..HEAD`.
        vec!["format-patch", "HEAD~3", "--stdout"],
        // Asymmetric range.
        vec!["format-patch", "HEAD~5..HEAD~2", "--stdout"],
        // Count combined with a tip committish (the rev is the tip, not exclude).
        vec!["format-patch", "-2", "HEAD~5", "--stdout"],
        // Count combined with a range keeps the last n of the range.
        vec!["format-patch", "-2", "HEAD~4..HEAD", "--stdout"],
    ] {
        assert_same_stdout(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn format_patch_numbering_and_options_match_git() {
    if !interop_enabled() {
        return;
    }
    let repo = build_repo("format-patch-options");

    for args in [
        // Forced numbering of a single patch and forced un-numbering of many.
        vec!["format-patch", "-1", "-n", "--stdout"],
        vec!["format-patch", "-3", "-N", "--stdout"],
        vec!["format-patch", "-3", "--numbered", "--stdout"],
        // Custom start number shifts both the `n/m` and the file index.
        vec!["format-patch", "-2", "--start-number", "5", "--stdout"],
        // Sign-off trailer (uses the runtime committer identity).
        vec!["format-patch", "-1", "--signoff", "--stdout"],
        vec!["format-patch", "-2", "--signoff", "--stdout"],
        // Stat toggles.
        vec!["format-patch", "-1", "--no-stat", "--stdout"],
        vec!["format-patch", "-1", "HEAD~7", "--no-stat", "--stdout"],
        // index-line width controls.
        vec!["format-patch", "-1", "HEAD~7", "--full-index", "--stdout"],
        vec!["format-patch", "-1", "HEAD~7", "--abbrev=16", "--stdout"],
        // Custom subject prefix and the RFC shortcut.
        vec![
            "format-patch",
            "-1",
            "--subject-prefix=PATCH v2",
            "--stdout",
        ],
        vec!["format-patch", "-1", "--rfc", "--stdout"],
        // Rename-detection toggle on the rename commit.
        vec!["format-patch", "-1", "HEAD~5", "--no-renames", "--stdout"],
        vec!["format-patch", "-1", "HEAD~5", "-M", "--stdout"],
    ] {
        assert_same_stdout(&repo, &args);
    }

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn format_patch_file_output_matches_git() {
    if !interop_enabled() {
        return;
    }
    let repo = build_repo("format-patch-files");

    // Default file naming (NNNN-<slug>.patch) and contents.
    assert_same_files(&repo, &["-3"]);
    // Numbered files (1, 2, ... with no slug).
    assert_same_files(&repo, &["-2", "--numbered-files"]);
    // Custom start number affects the file index.
    assert_same_files(&repo, &["-2", "--start-number", "10"]);
    // A single patch (unnumbered subject, still a NNNN- file).
    assert_same_files(&repo, &["-1"]);
    // The whole history, exercising the slug for many subjects including the
    // long-subject commit (truncated to git's 52-character cap).
    assert_same_files(&repo, &["-9"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}

#[test]
fn format_patch_unknown_revision_matches_git() {
    if !interop_enabled() {
        return;
    }
    let repo = build_repo("format-patch-unknown");

    // Unknown revision: identical fatal stderr and exit 128.
    assert_same_stdout(&repo, &["format-patch", "no-such-rev", "--stdout"]);

    fs::remove_dir_all(repo.parent().expect("test operation should succeed")).ok();
}
