//! Differential interop tests for `git mktag` vs the system `git` binary.
//!
//! Each test builds a temp repository with the real `git` binary under a fixed
//! identity/date environment, then feeds the same tag payload on stdin to both
//! `git mktag` and `sley mktag` and asserts that stdout, stderr, and the exit
//! code match byte-for-byte. Both binaries run in the *same* repository, so the
//! tagged objects (and, for a valid payload, the resulting verbatim tag object)
//! are identical and the printed object ids can be compared directly. The whole
//! file is gated on `git --version` succeeding, so it is a no-op where git is
//! absent.
//!
//! Coverage spans the success path (verbatim write — including an uppercase
//! `object` SHA and a message with no trailing newline — for commit/blob/tree/tag
//! targets), the tagged-object checks (missing object, type mismatch), the full
//! fsck catalogue (structure, header order, NUL/termination, tag-name and tagger
//! identity rules), the `--strict`/`--no-strict` severity split, and the CLI's
//! usage/option-error handling (`-h`, unknown option/switch, `--strict=<v>`).
//!
//! This mirrors the structure of `tests/verify_tag.rs` and `tests/mktree.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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

/// The fixed identity/date environment the task pins, applied to every command
/// (both `git` and `sley`) so commit/tag object ids are reproducible.
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

/// Run `program` with `args` and `stdin`, under the fixed environment.
fn run_env_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
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

fn sley() -> &'static str {
    sley_testkit::sley_bin!()
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Capture the trimmed stdout of a `git` command (e.g. `rev-parse`), aborting on
/// failure.
fn git_capture(cwd: &Path, args: &[&str]) -> String {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8 output")
        .trim()
        .to_string()
}

/// Assert `git` and `sley` produce byte-identical stdout, identical stderr, and
/// the same exit code for `mktag args` with `stdin` piped in `repo`. Runs git
/// first (so a valid payload's tag object exists before sley writes the same
/// bytes — an idempotent no-op that prints the same id).
fn assert_same_mktag(repo: &Path, args: &[&str], stdin: &[u8]) {
    let mut full = vec!["mktag"];
    full.extend_from_slice(args);
    let g = run_env_stdin(sley_testkit::oracle_git(), repo, &full, stdin);
    let r = run_env_stdin(sley(), repo, &full, stdin);
    let label = format!("args={args:?} stdin={:?}", String::from_utf8_lossy(stdin));
    assert_eq!(
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&g.stdout),
        "stdout differs for {label}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&r.stderr),
        String::from_utf8_lossy(&g.stderr),
        "stderr differs for {label}"
    );
    assert_eq!(
        r.status.code(),
        g.status.code(),
        "exit code differs for {label}\nsley stdout: {}\nsley stderr: {}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr),
    );
}

/// A small repo with one commit, exposing the commit/tree/blob object ids used to
/// build tag payloads.
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    commit: String,
    tree: String,
    blob: String,
}

fn build_fixture(name: &str) -> Fixture {
    let root = unique_temp_dir(name);
    let repo = root.join("repo");
    git_ok(
        &root,
        &["init", "-q", "-b", "main", repo.to_str().expect("utf8")],
    );
    fs::write(repo.join("file.txt"), "content\n").expect("write file");
    git_ok(&repo, &["add", "file.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "initial"]);

    let commit = git_capture(&repo, &["rev-parse", "HEAD"]);
    let tree = git_capture(&repo, &["rev-parse", "HEAD^{tree}"]);
    let blob = git_capture(&repo, &["rev-parse", "HEAD:file.txt"]);
    Fixture {
        root,
        repo,
        commit,
        tree,
        blob,
    }
}

/// A well-formed tagger line for the fixed identity.
const TAGGER: &str = "tagger Tester <tester@example.com> 1790000000 -0500";

#[test]
fn mktag_valid_payloads_match_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-valid");

    let commit = &fx.commit;
    let tree = &fx.tree;
    let blob = &fx.blob;
    let upper = commit.to_ascii_uppercase();

    let cases: Vec<Vec<u8>> = vec![
        // Commit target with a message.
        format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n\nrelease notes\n").into_bytes(),
        // No message body (headers only, tagger present).
        format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n").into_bytes(),
        // Message with no trailing newline: must be preserved verbatim.
        format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n\nno trailing newline")
            .into_bytes(),
        // Blob target.
        format!("object {blob}\ntype blob\ntag blob-tag\n{TAGGER}\n\ntag a blob\n").into_bytes(),
        // Tree target.
        format!("object {tree}\ntype tree\ntag tree-tag\n{TAGGER}\n\ntag a tree\n").into_bytes(),
        // Uppercase object SHA: git stores it verbatim (a distinct object id).
        format!("object {upper}\ntype commit\ntag v1.0\n{TAGGER}\n\nupper\n").into_bytes(),
        // A multi-level tag name and a "weird but valid" name.
        format!("object {commit}\ntype commit\ntag releases/v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntype commit\ntag -leading-dash\n{TAGGER}\n\nm\n").into_bytes(),
        // NUL byte inside the message body is allowed (header region is clean).
        {
            let mut v =
                format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n\nbo").into_bytes();
            v.push(0);
            v.extend_from_slice(b"dy\n");
            v
        },
    ];

    for payload in &cases {
        assert_same_mktag(&fx.repo, &[], payload);
        // The same payload under --no-strict and --strict is still valid.
        assert_same_mktag(&fx.repo, &["--no-strict"], payload);
        assert_same_mktag(&fx.repo, &["--strict"], payload);
    }

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_tagged_object_checks_match_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-object-checks");
    let commit = &fx.commit;
    let blob = &fx.blob;
    let missing = "0000000000000000000000000000000000000000";

    let cases: Vec<Vec<u8>> = vec![
        // Tagged object does not exist.
        format!("object {missing}\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        // Declared type does not match the actual object's type.
        format!("object {commit}\ntype blob\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {blob}\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntype tree\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
    ];
    for payload in &cases {
        assert_same_mktag(&fx.repo, &[], payload);
    }

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_structure_fsck_errors_match_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-structure");
    let commit = &fx.commit;

    let cases: Vec<Vec<u8>> = vec![
        // Missing / out-of-order headers.
        format!("type commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntype commit\n{TAGGER}\n\nm\n").into_bytes(),
        format!("type commit\nobject {commit}\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        // Wrong object-line format.
        format!("object zzzz\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object 1234\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit} \ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}a\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        // Bad / non-canonical type value.
        format!("object {commit}\ntype bogus\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntype Commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        format!("object {commit}\ntype commit x\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        // Capitalized / malformed header prefixes.
        format!("Object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes(),
        // Truncated input (unterminated header) and empty input.
        format!("object {commit}\ntype commit\ntag v1").into_bytes(),
        Vec::new(),
        // Extra header line after the tagger.
        format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\nfoo bar\n\nm\n").into_bytes(),
        // NUL inside the header region.
        {
            let mut v = format!("object {commit}\ntype commit\ntag v").into_bytes();
            v.push(0);
            v.extend_from_slice(format!("1\n{TAGGER}\n\nm\n").as_bytes());
            v
        },
    ];
    for payload in &cases {
        // Default (strict) and --no-strict, since structure errors are
        // error-severity and abort in both modes.
        assert_same_mktag(&fx.repo, &[], payload);
        assert_same_mktag(&fx.repo, &["--no-strict"], payload);
    }

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_tag_name_fsck_matches_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-tagname");
    let commit = &fx.commit;

    // Tag names that fail check_refname_format (one-level). badTagName is
    // warning-severity, so test both strict (abort) and --no-strict (written).
    let bad_names = [
        "has space",
        "a..b",
        "foo.lock",
        "ends/with.lock",
        ".leadingdot",
        "trailingdot.",
        "a.",
        "/leadingslash",
        "trailingslash/",
        "double//slash",
        "at@{brace",
        "tilde~x",
        "caret^x",
        "colon:x",
        "question?x",
        "star*x",
        "bracket[x",
        "back\\slash",
        ".",
    ];
    for name in bad_names {
        let payload =
            format!("object {commit}\ntype commit\ntag {name}\n{TAGGER}\n\nm\n").into_bytes();
        assert_same_mktag(&fx.repo, &[], &payload);
        assert_same_mktag(&fx.repo, &["--no-strict"], &payload);
    }

    // An empty tag name (the `tag ` prefix with nothing after it).
    let empty = format!("object {commit}\ntype commit\ntag \n{TAGGER}\n\nm\n").into_bytes();
    assert_same_mktag(&fx.repo, &[], &empty);
    assert_same_mktag(&fx.repo, &["--no-strict"], &empty);

    // Tag names that are accepted (sanity within the same harness).
    for name in ["v1.0", "a/b/c", "x.lock.y", "@", "weird+name=ok"] {
        let payload =
            format!("object {commit}\ntype commit\ntag {name}\n{TAGGER}\n\nm\n").into_bytes();
        assert_same_mktag(&fx.repo, &[], &payload);
    }

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_tagger_fsck_matches_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-tagger");
    let commit = &fx.commit;

    // Each entry is the text following `tagger ` (or, for the missing case, the
    // whole tagger line is omitted via the helper below).
    let bad_idents = [
        "Tester",                                // missingEmail
        "<tester@example.com> 1 -0500",          // missingNameBeforeEmail
        "ab<tester@example.com> 1 -0500",        // missingSpaceBeforeEmail
        "Te>st <tester@example.com> 1 -0500",    // badName
        "ab <tester@example.com 1 -0500",        // badEmail (no closing >)
        "ab <a<b@example.com> 1 -0500",          // badEmail (nested <)
        "Tester <tester@example.com>1 -0500",    // missingSpaceBeforeDate
        "Tester <tester@example.com> abc -0500", // badDate
        "Tester <tester@example.com> 12x -0500", // badDate
        "Tester <tester@example.com> 007 -0500", // zeroPaddedDate
        "Tester <tester@example.com> 00 -0500",  // zeroPaddedDate
        "Tester <tester@example.com> 1 0500",    // badTimezone (no sign)
        "Tester <tester@example.com> 1 +050",    // badTimezone (too short)
        "Tester <tester@example.com> 1 +05000",  // badTimezone (too long)
        "Tester <tester@example.com> 1 +05ab",   // badTimezone (non-digit)
        "Tester <tester@example.com> 1 +0500x",  // badTimezone (trailing junk)
    ];
    for ident in bad_idents {
        let payload =
            format!("object {commit}\ntype commit\ntag v1.0\ntagger {ident}\n\nm\n").into_bytes();
        assert_same_mktag(&fx.repo, &[], &payload);
        assert_same_mktag(&fx.repo, &["--no-strict"], &payload);
    }

    // Idents that are accepted: empty name (leading space), empty email, the
    // single-zero date.
    for ident in [
        " <tester@example.com> 1790000000 -0500",
        "Tester <> 1790000000 -0500",
        "Tester <tester@example.com> 0 +0000",
    ] {
        let payload =
            format!("object {commit}\ntype commit\ntag v1.0\ntagger {ident}\n\nm\n").into_bytes();
        assert_same_mktag(&fx.repo, &[], &payload);
    }

    // Missing tagger line entirely: warning-severity (strict aborts, --no-strict
    // writes the tag).
    let no_tagger = format!("object {commit}\ntype commit\ntag v1.0\n\nm\n").into_bytes();
    assert_same_mktag(&fx.repo, &[], &no_tagger);
    assert_same_mktag(&fx.repo, &["--no-strict"], &no_tagger);

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_multiple_warnings_ordering_matches_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-multi-warn");
    let commit = &fx.commit;

    // Bad tag name (warning) followed by a missing tagger (warning): under
    // --no-strict git prints both warnings in order and still writes the tag;
    // under strict it stops at the first.
    let two_warnings = format!("object {commit}\ntype commit\ntag has space\n\nm\n").into_bytes();
    assert_same_mktag(&fx.repo, &[], &two_warnings);
    assert_same_mktag(&fx.repo, &["--no-strict"], &two_warnings);

    // Missing tagger followed by an extra header line: two warnings in order.
    let missing_tagger_extra =
        format!("object {commit}\ntype commit\ntag v1.0\nfoo bar\n\nm\n").into_bytes();
    assert_same_mktag(&fx.repo, &[], &missing_tagger_extra);
    assert_same_mktag(&fx.repo, &["--no-strict"], &missing_tagger_extra);

    // A warning (bad tag name) before an error (bad type): the error wins and
    // aborts even under --no-strict.
    let warn_then_error =
        format!("object {commit}\ntype bogus\ntag has space\n{TAGGER}\n\nm\n").into_bytes();
    assert_same_mktag(&fx.repo, &[], &warn_then_error);
    assert_same_mktag(&fx.repo, &["--no-strict"], &warn_then_error);

    let _ = fs::remove_dir_all(&fx.root);
}

#[test]
fn mktag_cli_usage_and_option_errors_match_git() {
    if !git_available() {
        return;
    }
    let fx = build_fixture("mktag-cli");
    let commit = &fx.commit;
    let valid = format!("object {commit}\ntype commit\ntag v1.0\n{TAGGER}\n\nm\n").into_bytes();

    // `-h` and `--help-all` print the short usage to stdout, exit 129. (`--help`
    // is intentionally excluded: git's main dispatcher routes it to the man page,
    // whose rendering — pager, terminal width, git version date — is not
    // hermetic, so it is unsuitable for a byte-for-byte differential.)
    for flag in [["-h"], ["--help-all"]] {
        assert_same_mktag(&fx.repo, &flag, &valid);
    }
    // Unknown option / switch: error + usage to stderr, exit 129.
    for flag in [["--bogus"], ["-x"], ["-s"], ["--strict-typo"]] {
        assert_same_mktag(&fx.repo, &flag, &valid);
    }
    // `--strict` / `--no-strict` taking a value: one-line error, no usage, exit 129.
    for flag in [["--strict=1"], ["--strict="], ["--no-strict=0"]] {
        assert_same_mktag(&fx.repo, &flag, &valid);
    }
    // A trailing positional operand is ignored; `--` ends option parsing.
    assert_same_mktag(&fx.repo, &["extra"], &valid);
    assert_same_mktag(&fx.repo, &["--", "--strict"], &valid);

    let _ = fs::remove_dir_all(&fx.root);
}
