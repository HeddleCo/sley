//! Differential tests for `git checkout-index` against the system `git` binary.
//!
//! Each case drives both upstream `git` and the `sley` binary against mirrored
//! repositories and asserts identical stdout, stderr, and exit codes (plus the
//! resulting worktree/index state). The whole module is gated on a working
//! system `git`, so it is a no-op where the reference binary is unavailable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const GIT_RS: &str = env!("CARGO_BIN_EXE_sley");

/// Fixed identity + dates so commit object ids are byte-identical across repos.
const IDENTITY_ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Tester"),
    ("GIT_AUTHOR_EMAIL", "tester@example.com"),
    ("GIT_COMMITTER_NAME", "Tester"),
    ("GIT_COMMITTER_EMAIL", "tester@example.com"),
    ("GIT_AUTHOR_DATE", "@1790000000 -0500"),
    ("GIT_COMMITTER_DATE", "@1790000000 -0500"),
];

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in IDENTITY_ENV {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in IDENTITY_ENV {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}\nrust stderr:\n{}\ngit stderr:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for {args:?}"
    );
}

/// Create the two mirrored repositories and return `(upstream, rust)`.
fn prepare_pair(name: &str, root: &Path) -> (PathBuf, PathBuf) {
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let _ = name;
    for repo in [&upstream, &rust] {
        prepare_repo(repo);
    }
    (upstream, rust)
}

fn prepare_repo(root: &Path) {
    run_success(sley_testkit::oracle_git(), root, &["init", "-q"]);
    run_success(sley_testkit::oracle_git(), root, &["config", "core.autocrlf", "false"]);
    fs::write(root.join("file.txt"), b"base\n").expect("write file");
    fs::create_dir_all(root.join("dir")).expect("create dir");
    fs::write(root.join("dir/nested.txt"), b"nested\n").expect("write nested");
    run_success(sley_testkit::oracle_git(), root, &["add", "file.txt", "dir/nested.txt"]);
    run_success(sley_testkit::oracle_git(), root, &["commit", "-q", "-m", "base"]);
}

/// Remove the worktree files sley and git will recreate, leaving the index.
fn clear_worktree(root: &Path, paths: &[&str]) {
    for path in paths {
        let _ = fs::remove_file(root.join(path));
    }
}

fn worktree_status(root: &Path) -> Vec<u8> {
    run_success(sley_testkit::oracle_git(), root, &["status", "--porcelain"])
}

fn staged_index(root: &Path) -> Vec<u8> {
    run_success(sley_testkit::oracle_git(), root, &["ls-files", "--stage"])
}

#[test]
fn checkout_index_all_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-all");
    let (upstream, rust) = prepare_pair("all", &root);
    {
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt", "dir/nested.txt"]);
        }
        let args = ["checkout-index", "-a"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "file content differed after checkout-index -a"
        );
        assert_eq!(
            fs::read(rust.join("dir/nested.txt")).expect("read rust nested"),
            fs::read(upstream.join("dir/nested.txt")).expect("read upstream nested"),
            "nested content differed after checkout-index -a"
        );
        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after checkout-index -a"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_existing_without_force_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-existing");
    let (upstream, rust) = prepare_pair("existing", &root);
    {
        // Leave the worktree populated so each entry already exists on disk.
        let args = ["checkout-index", "-a"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_force_overwrites_match_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-force");
    let (upstream, rust) = prepare_pair("force", &root);
    {
        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"dirty\n").expect("dirty worktree");
        }
        let args = ["checkout-index", "-a", "-f"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            b"base\n",
            "checkout-index -f did not restore index content"
        );
        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after checkout-index -a -f"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_explicit_paths_match_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-paths");
    let (upstream, rust) = prepare_pair("paths", &root);
    {
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt", "dir/nested.txt"]);
        }
        let args = ["checkout-index", "file.txt", "dir/nested.txt"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "explicit-path content differed"
        );
        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after explicit-path checkout-index"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_missing_path_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-missing");
    let (upstream, rust) = prepare_pair("missing", &root);
    {
        let args = ["checkout-index", "absent.txt"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        // Quiet form suppresses the warning but keeps the nonzero exit.
        let quiet_args = ["checkout-index", "-q", "absent.txt"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &quiet_args);
        let actual = run(GIT_RS, &rust, &quiet_args);
        assert_same_output(actual, expected, &quiet_args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_prefix_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-prefix");
    let (upstream, rust) = prepare_pair("prefix", &root);
    {
        for repo in [&upstream, &rust] {
            fs::create_dir_all(repo.join("out")).expect("create out dir");
        }
        let args = [
            "checkout-index",
            "--prefix=out/",
            "file.txt",
            "dir/nested.txt",
        ];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("out/file.txt")).expect("read rust prefixed file"),
            fs::read(upstream.join("out/file.txt")).expect("read upstream prefixed file"),
            "prefixed file content differed"
        );
        assert_eq!(
            fs::read(rust.join("out/dir/nested.txt")).expect("read rust prefixed nested"),
            fs::read(upstream.join("out/dir/nested.txt")).expect("read upstream prefixed nested"),
            "prefixed nested content differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_stdin_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-stdin");
    let (upstream, rust) = prepare_pair("stdin", &root);
    {
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt", "dir/nested.txt"]);
        }
        let args = ["checkout-index", "--stdin"];
        let stdin = b"file.txt\ndir/nested.txt\n";
        let expected = run_with_stdin(sley_testkit::oracle_git(), &upstream, &args, stdin);
        let actual = run_with_stdin(GIT_RS, &rust, &args, stdin);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after checkout-index --stdin"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_stdin_nul_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-stdin-nul");
    let (upstream, rust) = prepare_pair("stdin-nul", &root);
    {
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt", "dir/nested.txt"]);
        }
        let args = ["checkout-index", "--stdin", "-z"];
        let stdin = b"file.txt\0dir/nested.txt\0";
        let expected = run_with_stdin(sley_testkit::oracle_git(), &upstream, &args, stdin);
        let actual = run_with_stdin(GIT_RS, &rust, &args, stdin);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after checkout-index --stdin -z"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_update_stat_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-update");
    let (upstream, rust) = prepare_pair("update", &root);
    {
        // Recreate worktree files so their stat info diverges from the index,
        // then `-u` should refresh stat data and leave a clean status.
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt"]);
        }
        let args = ["checkout-index", "-u", "-f", "file.txt"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            staged_index(&rust),
            staged_index(&upstream),
            "index entries differed after checkout-index -u"
        );
        assert_eq!(
            worktree_status(&rust),
            worktree_status(&upstream),
            "status differed after checkout-index -u"
        );
        // diff-files should agree (both clean) once stat info is refreshed.
        let diff_args = ["diff-files", "--name-only"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &diff_args);
        let actual = run(sley_testkit::oracle_git(), &rust, &diff_args);
        assert_same_output(actual, expected, &diff_args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_mix_all_and_paths_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-mix-all");
    let (upstream, rust) = prepare_pair("mix-all", &root);
    {
        let args = ["checkout-index", "-a", "file.txt"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_mix_stdin_and_paths_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-mix-stdin");
    let (upstream, rust) = prepare_pair("mix-stdin", &root);
    {
        let args = ["checkout-index", "--stdin", "file.txt"];
        let expected = run_with_stdin(sley_testkit::oracle_git(), &upstream, &args, b"");
        let actual = run_with_stdin(GIT_RS, &rust, &args, b"");
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_executable_and_symlink_modes_match_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-modes");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            run_success(sley_testkit::oracle_git(), repo, &["init", "-q"]);
            run_success(sley_testkit::oracle_git(), repo, &["config", "core.autocrlf", "false"]);
            fs::write(repo.join("run.sh"), b"#!/bin/sh\necho hi\n").expect("write script");
            set_executable(&repo.join("run.sh"));
            std::os::unix::fs::symlink("run.sh", repo.join("link")).expect("create symlink");
            fs::write(repo.join("plain.txt"), b"plain\n").expect("write plain");
            run_success(sley_testkit::oracle_git(), repo, &["add", "run.sh", "link", "plain.txt"]);
            run_success(sley_testkit::oracle_git(), repo, &["commit", "-q", "-m", "modes"]);
            clear_worktree(repo, &["run.sh", "link", "plain.txt"]);
        }
        let args = ["checkout-index", "-a"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run(GIT_RS, &rust, &args);
        assert_same_output(actual, expected, &args);

        // Symlink stays a symlink with the same target.
        let rust_link = fs::symlink_metadata(rust.join("link")).expect("rust link metadata");
        assert!(
            rust_link.file_type().is_symlink(),
            "checkout-index did not recreate the symlink"
        );
        assert_eq!(
            fs::read_link(rust.join("link")).expect("read rust link"),
            fs::read_link(upstream.join("link")).expect("read upstream link"),
            "symlink target differed"
        );
        // Executable bit is preserved on the script.
        assert_eq!(
            is_executable(&rust.join("run.sh")),
            is_executable(&upstream.join("run.sh")),
            "executable bit differed after checkout-index"
        );
        assert_eq!(
            staged_index(&rust),
            staged_index(&upstream),
            "index differed after mode checkout-index"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_index_subdir_all_matches_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("checkout-index-subdir");
    let (upstream, rust) = prepare_pair("subdir", &root);
    {
        // `-a` from a subdirectory only checks out entries beneath it, written
        // to their repository-relative paths.
        for repo in [&upstream, &rust] {
            clear_worktree(repo, &["file.txt", "dir/nested.txt"]);
        }
        let args = ["checkout-index", "-a"];
        let upstream_sub = upstream.join("dir");
        let rust_sub = rust.join("dir");
        let expected = run(sley_testkit::oracle_git(), &upstream_sub, &args);
        let actual = run(GIT_RS, &rust_sub, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            rust.join("dir/nested.txt").exists(),
            upstream.join("dir/nested.txt").exists(),
            "subdir nested presence differed"
        );
        assert_eq!(
            rust.join("file.txt").exists(),
            upstream.join("file.txt").exists(),
            "subdir checkout should not write root-level entry"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("set executable");
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
