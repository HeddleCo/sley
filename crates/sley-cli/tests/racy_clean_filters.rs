//! Regression tests: index-vs-worktree classification must consult the
//! smudge/clean filters when cached stat info is stale ("racy" files), or
//! committed CRLF/clean-filter files phantom-show as modified.
//!
//! Scenario: a repo with `* text=auto` commits a CRLF file (index stores the
//! LF-cleaned blob, worktree keeps CRLF). A byte-identical rewrite of the file
//! invalidates the cached stat, forcing content re-classification. Call sites
//! that skip the `StatCleanFilterValidator` hash raw CRLF worktree bytes,
//! mismatch the LF index blob, and report a phantom modification. Oracle `git`
//! reports clean (it re-checks through the text conversion).
//!
//! Covered call sites:
//! * `commit -v -v` editor template — "Changes not staged for commit:" diff
//!   (`append_commit_diff_index_patch` with `worktree = true`).
//! * `difftool` entry collection (`collect_difftool_entries`, plain
//!   index-vs-worktree mode).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const CRLF_CONTENT: &[u8] = b"hello\r\nworld\r\n";

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    sley_testkit::apply_hermetic_git_env(&mut command);
    command
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    sley_testkit::apply_hermetic_git_env(&mut command);
    sley_testkit::apply_standard_git_identity_env(&mut command);
    command
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Create a repo whose HEAD contains a CRLF worktree file stored as an
/// LF-normalized blob (`* text=auto`), then invalidate its cached stat with a
/// byte-identical rewrite so classification must fall back to filter
/// validation. Also leaves `staged.txt` for callers that need staged content.
fn prepare_racy_clean_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo dir");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join(".gitattributes"), b"* text=auto\n").expect("write attributes");
    fs::write(root.join("crlf.txt"), CRLF_CONTENT).expect("write crlf fixture");
    fs::write(root.join("staged.txt"), b"one\n").expect("write staged fixture");
    // The add warning about CRLF normalization is expected noise.
    let _ = run_output(sley_testkit::oracle_git(), root, &["add", "-A"]);
    let commit =
        run_output_with_identity(sley_testkit::oracle_git(), root, &["commit", "-m", "init"]);
    assert!(
        commit.status.success(),
        "initial commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    // Byte-identical rewrite: same size and content, fresh mtime/ctime. The
    // cached stat no longer proves cleanliness, matching git's stale-stat
    // fallback into `ce_compare_data` (which consults conversion).
    fs::write(root.join("crlf.txt"), CRLF_CONTENT).expect("rewrite crlf fixture");

    // Precondition shared with the oracle: this state is clean, not modified.
    let porcelain = run_output(sley_testkit::oracle_git(), root, &["status", "--porcelain"]);
    assert_eq!(
        porcelain.stdout, b"",
        "oracle must treat the byte-identical CRLF rewrite as clean"
    );
}

fn status_porcelain(program: &str, root: &Path) -> Vec<u8> {
    run_output(program, root, &["status", "--porcelain"]).stdout
}

#[test]
fn difftool_skips_racy_clean_crlf_file() {
    let git_root = unique_temp_dir("racy-difftool-git");
    let sley_root = unique_temp_dir("racy-difftool-sley");
    let result = std::panic::catch_unwind(|| {
        prepare_racy_clean_repo(&git_root);
        prepare_racy_clean_repo(&sley_root);

        let git_log = git_root.join("launches.log");
        let sley_log = sley_root.join("launches.log");
        let extcmd_for = |log: &Path| format!("echo launch >> {}", log.display());

        // Phase 1: racy-clean state — no tool may launch on either binary.
        run_success(
            sley_testkit::oracle_git(),
            &git_root,
            &["difftool", "--no-prompt", "--extcmd", &extcmd_for(&git_log)],
        );
        run_success(
            sley_testkit::sley_bin!(),
            &sley_root,
            &[
                "difftool",
                "--no-prompt",
                "--extcmd",
                &extcmd_for(&sley_log),
            ],
        );
        assert!(!git_log.exists(), "oracle launched a tool for clean tree");
        assert!(
            !sley_log.exists(),
            "sley difftool launched a tool for a racy-clean CRLF file (phantom modification)"
        );

        // Phase 2: real content change — exactly one launch each. Guards the
        // harness itself against a silently broken logging extcmd.
        fs::write(git_root.join("crlf.txt"), b"hello\r\nworld\r\nmore\r\n")
            .expect("modify oracle crlf fixture");
        fs::write(sley_root.join("crlf.txt"), b"hello\r\nworld\r\nmore\r\n")
            .expect("modify sley crlf fixture");
        run_success(
            sley_testkit::oracle_git(),
            &git_root,
            &["difftool", "--no-prompt", "--extcmd", &extcmd_for(&git_log)],
        );
        run_success(
            sley_testkit::sley_bin!(),
            &sley_root,
            &[
                "difftool",
                "--no-prompt",
                "--extcmd",
                &extcmd_for(&sley_log),
            ],
        );
        assert_eq!(
            fs::read_to_string(&git_log)
                .expect("oracle log")
                .lines()
                .count(),
            1,
            "oracle must launch once for a genuinely modified file"
        );
        // Oracle's extcmd receives the materialized paths as arguments, sley's
        // does not; compare launch counts, not bytes.
        assert_eq!(
            fs::read_to_string(&sley_log)
                .expect("sley log")
                .lines()
                .count(),
            1,
            "sley difftool must still diff genuinely modified files"
        );

        assert_eq!(
            status_porcelain(sley_testkit::sley_bin!(), &sley_root),
            status_porcelain(sley_testkit::oracle_git(), &git_root),
        );
    });
    let _ = fs::remove_dir_all(&git_root);
    let _ = fs::remove_dir_all(&sley_root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn commit_verbose_template_omits_racy_clean_crlf_file() {
    let git_root = unique_temp_dir("racy-commit-git");
    let sley_root = unique_temp_dir("racy-commit-sley");
    let result = std::panic::catch_unwind(|| {
        prepare_racy_clean_repo(&git_root);
        prepare_racy_clean_repo(&sley_root);

        // Stage a genuine change so editor-mode commit proceeds past the
        // nothing-to-commit gate; the racy crlf.txt stays unstaged.
        for root in [&git_root, &sley_root] {
            fs::write(root.join("staged.txt"), b"two\n").expect("stage fixture");
            run_success(sley_testkit::oracle_git(), root, &["add", "staged.txt"]);
        }

        // Capturing editor: snapshots COMMIT_EDITMSG then fails, so the commit
        // aborts but the verbose template survives in `<editmsg>.cap`.
        for root in [&git_root, &sley_root] {
            let editor = root.join(".git/FAKE_EDITOR");
            fs::write(&editor, b"#!/bin/sh\ncp \"$1\" \"$1.cap\"\nexit 1\n").expect("write editor");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = fs::metadata(&editor)
                    .expect("editor metadata")
                    .permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&editor, permissions).expect("chmod editor");
            }
        }

        for root in [&git_root, &sley_root] {
            let program = if root == &git_root {
                sley_testkit::oracle_git()
            } else {
                sley_testkit::sley_bin!()
            };
            let mut command = Command::new(program);
            sley_testkit::apply_hermetic_git_env(&mut command);
            sley_testkit::apply_standard_git_identity_env(&mut command);
            command
                .current_dir(root)
                .args(["commit", "-v", "-v"])
                .env("GIT_EDITOR", root.join(".git/FAKE_EDITOR"))
                .output()
                .expect("run commit");
            // Editor exits 1; both binaries must abort without committing.
        }

        let expected = fs::read_to_string(git_root.join(".git/COMMIT_EDITMSG.cap"))
            .expect("oracle template captured");
        let actual = fs::read_to_string(sley_root.join(".git/COMMIT_EDITMSG.cap"))
            .expect("sley template captured");
        assert!(
            expected.contains("staged.txt") && !expected.contains("crlf.txt"),
            "oracle fixture drifted (expected staged.txt, no crlf.txt):\n{expected}"
        );
        // Targeted rather than whole-template parity: sley and oracle render
        // empty/unstaged verbose-diff section headers differently (pre-existing
        // cosmetic divergence outside this regression's scope).
        assert!(
            actual.contains("staged.txt"),
            "sley template missing staged sanity content:\n{actual}"
        );
        assert!(
            !actual.contains("crlf.txt"),
            "sley commit template shows phantom modification of racy-clean CRLF file:\n{actual}"
        );
        assert!(
            !actual.contains("diff --git i/"),
            "sley commit template shows a spurious unstaged diff:\n{actual}"
        );
    });
    let _ = fs::remove_dir_all(&git_root);
    let _ = fs::remove_dir_all(&sley_root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
