//! Differential interop tests for `git patch-id` vs the system git binary.
//!
//! `git patch-id` reads a diff on stdin and prints `<patch-id> <commit-id>` for
//! each patch it finds. Each test builds a temp repository with the real `git`
//! binary, produces patch text with `git diff`/`git show`/`git log -p`/
//! `git format-patch`, then feeds that identical text to both `git patch-id` and
//! `sley patch-id` and asserts stdout, stderr, and the exit code match
//! byte-for-byte. Because both binaries see the same objects built under a fixed
//! identity/date environment, the embedded commit ids are identical and compare
//! directly.
//!
//! Coverage spans the patch shapes that exercise the algorithm's branches:
//! single- and multi-file diffs (the latter distinguishing the order-independent
//! `--stable` id from the order-sensitive default), binary patches (both the
//! `GIT binary patch` literal form and the `Binary files differ` form), renames
//! plus mode changes plus add/delete, `format-patch` output (whose `From <oid>`
//! line supplies the commit id), multi-commit `log -p` streams (where one commit
//! id threads to the next patch), a SHA-256 repository (64-hex ids), the
//! `--stable`/`--unstable`/`--verbatim` flags and their mutual exclusion, the
//! `patchid.stable` config, and usage/option-error handling. The whole file is
//! gated on `git --version` succeeding, so it is a no-op where git is absent.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic per-process counter so concurrently-running tests (cargo runs them
/// on parallel threads in one process) never collide on a temp path even when
/// their nanosecond timestamps coincide.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}-{seq}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// The fixed identity/date environment the task pins, applied to every command
/// (both `git` and `sley`) so commit object ids are reproducible.
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

/// Run a command feeding `stdin`, under the same pinned environment.
fn run_env_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
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
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin is piped"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a repository-building `git` command at a specific author/committer date so
/// commits get distinct, deterministic timestamps (and thus distinct ids).
fn git_at(cwd: &Path, args: &[&str], date: &str) {
    let output = Command::new(sley_testkit::oracle_git())
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

fn git_rs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Whether the local git can create SHA-256 repositories (older builds cannot).
fn git_supports_sha256(root: &Path) -> bool {
    let probe = root.join("sha256-probe");
    let output = git(
        root,
        &[
            "init",
            "-q",
            "--template=",
            "--object-format=sha256",
            probe.to_str().expect("utf8"),
        ],
    );
    let supported = output.status.success();
    let _ = fs::remove_dir_all(&probe);
    supported
}

/// Capture the stdout of a `git` command as raw bytes, aborting on failure. Used
/// to obtain patch text (`diff`, `show`, `log -p`, `format-patch`).
fn git_capture_bytes(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Assert git and sley produce byte-identical stdout/stderr and the same exit
/// code for `patch-id args` fed `stdin`.
fn assert_same_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) {
    let g = run_env_with_stdin(sley_testkit::oracle_git(), cwd, args, stdin);
    let r = run_env_with_stdin(git_rs_bin(), cwd, args, stdin);
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
        "exit code differs for {args:?}\nsley stdout: {}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr),
    );
}

/// Assert agreement for the same patch under the default invocation plus each of
/// the three mode flags. This is the workhorse for patch-shape coverage.
fn assert_all_modes(cwd: &Path, patch: &[u8]) {
    for mode in [
        vec!["patch-id"],
        vec!["patch-id", "--stable"],
        vec!["patch-id", "--unstable"],
        vec!["patch-id", "--verbatim"],
    ] {
        assert_same_stdin(cwd, &mode, patch);
    }
}

fn write_commit(repo: &Path, file: &str, contents: &str, message: &str, date: &str) {
    fs::write(repo.join(file), contents).unwrap_or_else(|err| panic!("write {file}: {err}"));
    git_ok(repo, &["add", file]);
    git_at(repo, &["commit", "-q", "-m", message], date);
}

/// Build a repository whose history exercises every diff shape patch-id cares
/// about, returning `(root, repo)`. The commits, in order:
///
/// 1. add `m.txt`, `z.txt`              (two text files)
/// 2. change both `m.txt` and `z.txt`   (multi-file hunk diff)
/// 3. add `bin.dat`                     (binary blob)
/// 4. change `bin.dat`                  (binary diff)
/// 5. add `big.txt` (30 lines)          (sets up a separate-hunk diff)
/// 6. change `big.txt` lines 2 and 29   (two non-adjacent hunks in one file)
/// 7. delete `z.txt`, rename `m.txt`->`r.txt` with edit, chmod `big.txt` +x
///    (rename + mode change + delete + content edit in one commit)
fn build_repo() -> (PathBuf, PathBuf) {
    let root = unique_temp_dir("patch-id");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "--template=",
            "-b",
            "main",
            repo.to_str().expect("utf8"),
        ],
    );

    // 1: two files.
    fs::write(repo.join("m.txt"), "mmm\nnnn\n").expect("write m.txt");
    fs::write(repo.join("z.txt"), "aaa\nbbb\n").expect("write z.txt");
    git_ok(&repo, &["add", "m.txt", "z.txt"]);
    git_at(
        &repo,
        &["commit", "-q", "-m", "add m and z"],
        "@1790000000 -0500",
    );

    // 2: change both.
    fs::write(repo.join("m.txt"), "mmmCHANGED\nnnn\n").expect("write m.txt");
    fs::write(repo.join("z.txt"), "aaa\nbbbCHANGED\n").expect("write z.txt");
    git_ok(&repo, &["add", "m.txt", "z.txt"]);
    git_at(
        &repo,
        &["commit", "-q", "-m", "change m and z"],
        "@1790000100 -0500",
    );

    // 3 + 4: binary add then change.
    fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, b'B', b'I', b'N']).expect("write bin.dat");
    git_ok(&repo, &["add", "bin.dat"]);
    git_at(
        &repo,
        &["commit", "-q", "-m", "add binary"],
        "@1790000200 -0500",
    );
    fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 4, b'N', b'E', b'W']).expect("write bin.dat");
    git_ok(&repo, &["add", "bin.dat"]);
    git_at(
        &repo,
        &["commit", "-q", "-m", "change binary"],
        "@1790000300 -0500",
    );

    // 5 + 6: a 30-line file, then two non-adjacent edits (separate hunks).
    let big: String = (1..=30).map(|i| format!("line{i}\n")).collect();
    write_commit(&repo, "big.txt", &big, "add big", "@1790000400 -0500");
    let big_edited: String = (1..=30)
        .map(|i| match i {
            2 => "CHANGED2\n".to_string(),
            29 => "CHANGED29\n".to_string(),
            other => format!("line{other}\n"),
        })
        .collect();
    write_commit(
        &repo,
        "big.txt",
        &big_edited,
        "edit big twice",
        "@1790000500 -0500",
    );

    // 7: rename m->r with edit, delete z, chmod big +x — all in one commit.
    fs::write(repo.join("nf.txt"), "fresh\ncontent\n").expect("write nf.txt");
    git_ok(&repo, &["rm", "-q", "z.txt"]);
    git_ok(&repo, &["mv", "m.txt", "r.txt"]);
    fs::write(repo.join("r.txt"), "mmmX\nnnnY\n").expect("write r.txt");
    let mode_target = repo.join("big.txt");
    set_executable(&mode_target);
    git_ok(&repo, &["add", "-A"]);
    git_at(
        &repo,
        &["commit", "-q", "-m", "rename mode delete"],
        "@1790000600 -0500",
    );

    (root, repo)
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("stat for chmod").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod +x");
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {
    // On non-unix the mode-change diff simply will not appear; the rest of the
    // commit still exercises rename/delete/add, and patch-id agreement holds.
}

#[test]
fn patch_id_single_and_multi_file_diffs_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // Single-file diff (z.txt between the first two commits).
    let single = git_capture_bytes(&repo, &["diff", "main~6", "main~5", "--", "z.txt"]);
    assert_all_modes(&repo, &single);

    // Multi-file diff: --stable is order-independent while the default is not, so
    // both the natural order and a manually reversed file order are checked.
    let multi = git_capture_bytes(&repo, &["diff", "main~6", "main~5"]);
    assert_all_modes(&repo, &multi);

    let m_only = git_capture_bytes(&repo, &["diff", "main~6", "main~5", "--", "m.txt"]);
    let z_only = git_capture_bytes(&repo, &["diff", "main~6", "main~5", "--", "z.txt"]);
    let mut reversed = z_only.clone();
    reversed.extend_from_slice(&m_only);
    assert_all_modes(&repo, &reversed);

    // Two non-adjacent hunks in one file (stable folds each hunk separately).
    let two_hunks = git_capture_bytes(&repo, &["diff", "main~2", "main~1", "--", "big.txt"]);
    assert_all_modes(&repo, &two_hunks);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_binary_diffs_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // `GIT binary patch` literal form (from `git diff --binary`).
    let binary = git_capture_bytes(&repo, &["diff", "--binary", "main~4", "main~3"]);
    assert_all_modes(&repo, &binary);

    // `Binary files a/.. and b/.. differ` form (plain `git diff`).
    let binary_files = git_capture_bytes(&repo, &["diff", "main~4", "main~3"]);
    assert_all_modes(&repo, &binary_files);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_rename_mode_and_delete_diff_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // The final commit mixes a rename-with-edit, a deletion, a new file, and (on
    // unix) a mode change — several header forms in one patch.
    let complex = git_capture_bytes(&repo, &["diff", "main~1", "main"]);
    assert_all_modes(&repo, &complex);

    // `git show` of the same commit prepends a `commit <oid>` line, so the patch
    // id carries that commit id instead of zeros.
    let shown = git_capture_bytes(&repo, &["show", "main"]);
    assert_all_modes(&repo, &shown);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_format_patch_and_log_streams_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    // `format-patch` output carries the commit id on a `From <oid> Mon Sep 17 …`
    // line, which patch-id must parse despite the trailing date text.
    let formatted = git_capture_bytes(&repo, &["format-patch", "-1", "--stdout", "main"]);
    assert_all_modes(&repo, &formatted);

    // A multi-commit `log -p` stream: each `commit <oid>` line seeds the *next*
    // emitted patch's id, exercising the cross-patch threading.
    let log_stream = git_capture_bytes(&repo, &["log", "-p", "-4"]);
    assert_all_modes(&repo, &log_stream);

    // Two diffs separated by free-form junk lines (no `commit`/`From` boundary).
    // The junk ends the first patch and the parser must resume at the second
    // `diff` header, emitting two ids — the cross-patch re-read path.
    let one_diff = git_capture_bytes(&repo, &["diff", "main~1", "main"]);
    let mut junked = one_diff.clone();
    junked.extend_from_slice(b"some trailing junk line\nanother junk line\n");
    junked.extend_from_slice(&one_diff);
    assert_all_modes(&repo, &junked);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_sha256_repository_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("patch-id-sha256");
    if !git_supports_sha256(&root) {
        let _ = fs::remove_dir_all(&root);
        return;
    }
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "--template=",
            "-b",
            "main",
            "--object-format=sha256",
            repo.to_str().expect("utf8"),
        ],
    );

    fs::write(repo.join("m.txt"), "mmm\nnnn\n").expect("write m.txt");
    fs::write(repo.join("z.txt"), "aaa\nbbb\n").expect("write z.txt");
    git_ok(&repo, &["add", "m.txt", "z.txt"]);
    git_at(&repo, &["commit", "-q", "-m", "c1"], "@1790000000 -0500");
    fs::write(repo.join("m.txt"), "mmmX\nnnn\n").expect("write m.txt");
    fs::write(repo.join("z.txt"), "aaa\nbbbX\n").expect("write z.txt");
    git_ok(&repo, &["add", "m.txt", "z.txt"]);
    git_at(&repo, &["commit", "-q", "-m", "c2"], "@1790000100 -0500");

    // In a SHA-256 repo both the patch id and the commit id are 64 hex chars.
    let diff = git_capture_bytes(&repo, &["diff", "HEAD~1", "HEAD"]);
    assert_all_modes(&repo, &diff);
    let formatted = git_capture_bytes(&repo, &["format-patch", "-1", "--stdout", "HEAD"]);
    assert_all_modes(&repo, &formatted);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_config_default_matches_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();
    let multi = git_capture_bytes(&repo, &["diff", "main~6", "main~5"]);

    // `patchid.stable` selects the default algorithm; an explicit flag overrides
    // it. Drive the config through `-c` (the same override channel git uses) so
    // the test is independent of any user/global config.
    for args in [
        vec!["-c", "patchid.stable=true", "patch-id"],
        vec!["-c", "patchid.stable=false", "patch-id"],
        vec!["-c", "patchid.stable=1", "patch-id"],
        vec!["-c", "patchid.stable=0", "patch-id"],
        // An explicit flag wins over the config.
        vec!["-c", "patchid.stable=true", "patch-id", "--unstable"],
        vec!["-c", "patchid.stable=false", "patch-id", "--stable"],
        // A non-boolean config value is fatal with a specific message.
        vec!["-c", "patchid.stable=maybe", "patch-id"],
    ] {
        assert_same_stdin(&repo, &args, &multi);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_option_and_usage_errors_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();
    let multi = git_capture_bytes(&repo, &["diff", "main~6", "main~5"]);

    for args in [
        // `-h` prints usage to stdout and exits 129. (`--help` is excluded: real
        // git execs the man page, which is not reproducible in a hermetic test.)
        vec!["patch-id", "-h"],
        // Unknown long option / short switch: error + usage on stderr, exit 129.
        vec!["patch-id", "--bogus"],
        vec!["patch-id", "-x"],
        // `--no-` forms are not negatable options here; treated as unknown.
        vec!["patch-id", "--no-stable"],
        // Mutually exclusive mode flags, in both orders (the message names the
        // later flag first).
        vec!["patch-id", "--stable", "--unstable"],
        vec!["patch-id", "--unstable", "--stable"],
        vec!["patch-id", "--stable", "--verbatim"],
        vec!["patch-id", "--verbatim", "--stable"],
        vec!["patch-id", "--unstable", "--verbatim"],
        vec!["patch-id", "--verbatim", "--unstable"],
        // Repeating the same flag is fine.
        vec!["patch-id", "--stable", "--stable"],
        vec!["patch-id", "--verbatim", "--verbatim"],
        // Unambiguous abbreviations are accepted (git's parse-options prefix
        // matching).
        vec!["patch-id", "--stab"],
        vec!["patch-id", "--unsta"],
        vec!["patch-id", "--verb"],
        // A trailing operand is ignored.
        vec!["patch-id", "extra-operand"],
        // `--` ends option parsing; the operand after it is ignored.
        vec!["patch-id", "--", "--stable"],
    ] {
        assert_same_stdin(&repo, &args, &multi);
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn patch_id_non_patch_and_empty_input_match_git() {
    if !git_available() {
        return;
    }
    let (root, repo) = build_repo();

    for input in [
        // Empty input: no output, exit 0.
        b"".as_slice(),
        // Plain text with no diff: no output, exit 0.
        b"hello world\nthis is not a patch at all\n".as_slice(),
        // A lone newline.
        b"\n".as_slice(),
    ] {
        for mode in [
            vec!["patch-id"],
            vec!["patch-id", "--stable"],
            vec!["patch-id", "--verbatim"],
        ] {
            assert_same_stdin(&repo, &mode, input);
        }
    }

    let _ = fs::remove_dir_all(&root);
}
