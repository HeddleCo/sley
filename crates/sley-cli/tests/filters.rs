//! Differential interop tests for content filters (core.autocrlf / .gitattributes
//! eol) wired into add / commit / status / checkout, vs the system git binary.

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

fn ok(program: &str, cwd: &Path, args: &[&str]) {
    let out = run_env(program, cwd, args);
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

const GIT_RS: &str = env!("CARGO_BIN_EXE_sley");

/// The blob bytes stored for `path` at HEAD (`cat-file -p HEAD:path`).
fn blob_at_head(program: &str, cwd: &Path, path: &str) -> Vec<u8> {
    run_env(program, cwd, &["cat-file", "-p", &format!("HEAD:{path}")]).stdout
}

fn porcelain(program: &str, cwd: &Path) -> Vec<u8> {
    run_env(program, cwd, &["status", "--porcelain"]).stdout
}

/// With `core.autocrlf=true`, the full loop must match git on both sides:
/// add normalizes CRLF->LF in the stored blob (clean), `status` reports the
/// CRLF worktree as clean (clean-compare), and a branch switch that
/// re-materializes the file restores CRLF (smudge).
#[test]
fn autocrlf_true_round_trip_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("autocrlf-true");
    let crlf: &[u8] = b"line one\r\nline two\r\nline three\r\n";
    let lf: &[u8] = b"line one\nline two\nline three\n";

    for program in [GIT_RS, sley_testkit::oracle_git()] {
        let repo = root.join(if program == sley_testkit::oracle_git() {
            "ref"
        } else {
            "cand"
        });
        ok(
            program,
            &root,
            &[
                "init",
                "-b",
                "main",
                "-q",
                repo.to_str().expect("test operation should succeed"),
            ],
        );
        ok(program, &repo, &["config", "core.autocrlf", "true"]);
        fs::write(repo.join("f.txt"), crlf).expect("test operation should succeed");

        // clean: add + commit store the LF-normalized blob.
        ok(program, &repo, &["add", "f.txt"]);
        ok(program, &repo, &["commit", "-m", "c1"]);
        assert_eq!(
            blob_at_head(program, &repo, "f.txt"),
            lf,
            "{program}: stored blob not LF-normalized"
        );

        // clean-compare: the CRLF worktree must read as clean.
        assert!(
            porcelain(program, &repo).is_empty(),
            "{program}: autocrlf worktree reported dirty: {}",
            String::from_utf8_lossy(&porcelain(program, &repo))
        );

        // smudge: move the file out of the tree on a side branch, then switch
        // back to main so checkout re-materializes it through the smudge filter.
        ok(program, &repo, &["checkout", "-b", "side"]);
        ok(program, &repo, &["rm", "f.txt"]);
        ok(program, &repo, &["commit", "-m", "drop"]);
        ok(program, &repo, &["checkout", "main"]);
        assert_eq!(
            fs::read(repo.join("f.txt")).expect("test operation should succeed"),
            crlf,
            "{program}: checkout did not smudge back to CRLF"
        );
    }

    // Both implementations stored the identical blob OID.
    assert_eq!(
        run_env(sley_testkit::oracle_git(), &root.join("ref"), &["rev-parse", "HEAD:f.txt"]).stdout,
        run_env(GIT_RS, &root.join("cand"), &["rev-parse", "HEAD:f.txt"]).stdout,
        "blob OID differs between git and sley under autocrlf"
    );

    fs::remove_dir_all(&root).ok();
}

/// A `.gitattributes` `text eol=crlf` attribute drives smudge to CRLF on
/// checkout (and clean to LF on add) even with core.autocrlf unset -- matching
/// git byte-for-byte, with the stored blob kept LF on both sides.
#[test]
fn gitattributes_eol_crlf_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("attr-eol");

    for program in [GIT_RS, sley_testkit::oracle_git()] {
        let repo = root.join(if program == sley_testkit::oracle_git() {
            "ref"
        } else {
            "cand"
        });
        ok(
            program,
            &root,
            &[
                "init",
                "-b",
                "main",
                "-q",
                repo.to_str().expect("test operation should succeed"),
            ],
        );
        fs::write(repo.join(".gitattributes"), "*.txt text eol=crlf\n")
            .expect("test operation should succeed");
        fs::write(repo.join("doc.txt"), "alpha\nbeta\n").expect("test operation should succeed");
        ok(program, &repo, &["add", "."]);
        ok(program, &repo, &["commit", "-m", "base"]);

        // Blob is LF; worktree reads clean even though it will be CRLF on disk.
        assert_eq!(blob_at_head(program, &repo, "doc.txt"), b"alpha\nbeta\n");

        ok(program, &repo, &["checkout", "-b", "side"]);
        ok(program, &repo, &["rm", "doc.txt"]);
        ok(program, &repo, &["commit", "-m", "drop"]);
        ok(program, &repo, &["checkout", "main"]);
    }

    assert_eq!(
        fs::read(root.join("ref").join("doc.txt")).expect("test operation should succeed"),
        fs::read(root.join("cand").join("doc.txt")).expect("test operation should succeed"),
        "eol=crlf smudge output differs from git"
    );
    assert_eq!(
        fs::read(root.join("cand").join("doc.txt")).expect("test operation should succeed"),
        b"alpha\r\nbeta\r\n",
        "eol=crlf did not produce CRLF in the worktree"
    );

    fs::remove_dir_all(&root).ok();
}

/// Sanity: with no autocrlf and no attributes (the default the rest of the
/// suite runs under), add preserves bytes exactly and produces git's OID -- the
/// filtered path is a pure passthrough.
#[test]
fn no_filter_default_is_passthrough() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("passthrough");
    let mixed: &[u8] = b"keep\r\nthese\nbytes\r\n";

    let cand = root.join("cand");
    let reference = root.join("ref");
    for (program, repo) in [(GIT_RS, &cand), (sley_testkit::oracle_git(), &reference)] {
        ok(
            program,
            &root,
            &[
                "init",
                "-b",
                "main",
                "-q",
                repo.to_str().expect("test operation should succeed"),
            ],
        );
        fs::write(repo.join("m.bin"), mixed).expect("test operation should succeed");
        ok(program, repo, &["add", "m.bin"]);
        ok(program, repo, &["commit", "-m", "c"]);
        assert_eq!(blob_at_head(program, repo, "m.bin"), mixed);
        assert!(porcelain(program, repo).is_empty());
    }
    assert_eq!(
        run_env(sley_testkit::oracle_git(), &reference, &["rev-parse", "HEAD:m.bin"]).stdout,
        run_env(GIT_RS, &cand, &["rev-parse", "HEAD:m.bin"]).stdout,
    );

    fs::remove_dir_all(&root).ok();
}
