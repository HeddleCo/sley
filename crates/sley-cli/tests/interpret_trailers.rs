//! Differential interop tests for `git interpret-trailers` against the system
//! `git` binary.
//!
//! `interpret-trailers` is pure text processing — it parses a commit message
//! from stdin or a file, edits its trailer block, and writes the result — so
//! these tests feed identical input/flags to both the reference `git` and the
//! `sley` build and require byte-identical stdout, stderr, and exit code.
//!
//! Inputs use realistic, subject-first commit messages (as real `git` produces),
//! which is the space `interpret-trailers` is meant for. A fixed identity and
//! date environment is set for parity with the rest of the suite even though
//! this command never touches object ids. The whole suite is skipped when
//! `git --version` is unavailable.

use std::fs;
use std::io::Write;
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

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Build a command for `program` with the fixed, hermetic environment shared by
/// the suite. `GIT_CONFIG_GLOBAL`/`SYSTEM` are pointed at `/dev/null` so a
/// developer's `~/.gitconfig` (e.g. a custom `trailer.*` or `core.commentChar`)
/// cannot perturb the reference output.
fn base_command(program: &str, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    command
}

fn git_rs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    base_command(program, cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = base_command(program, cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin)
        .expect("write stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

/// Feed `stdin` plus `args` to both binaries and require identical results.
fn assert_same_stdin(args: &[&str], stdin: &[u8]) {
    if !git_available() {
        return;
    }
    let cwd = std::env::temp_dir();
    let mut git_args = vec!["interpret-trailers"];
    git_args.extend_from_slice(args);
    let mut rs_args = vec!["interpret-trailers"];
    rs_args.extend_from_slice(args);

    let expected = run_with_stdin(sley_testkit::oracle_git(), &cwd, &git_args, stdin);
    let actual = run_with_stdin(git_rs_bin(), &cwd, &rs_args, stdin);

    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit code differed for {args:?} on stdin {:?}\n git stderr: {}\n rs  stderr: {}",
        String::from_utf8_lossy(stdin),
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for {args:?} on stdin {:?}",
        String::from_utf8_lossy(stdin),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for {args:?} on stdin {:?}",
        String::from_utf8_lossy(stdin),
    );
}

/// Run `args` (no stdin) in `cwd` against both binaries and require identical
/// results. Used for the `-h`/error cases.
fn assert_same_args(cwd: &Path, args: &[&str]) {
    if !git_available() {
        return;
    }
    let mut git_args = vec!["interpret-trailers"];
    git_args.extend_from_slice(args);
    let mut rs_args = vec!["interpret-trailers"];
    rs_args.extend_from_slice(args);

    let expected = run(sley_testkit::oracle_git(), cwd, &git_args);
    let actual = run(git_rs_bin(), cwd, &rs_args);

    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit code differed for {args:?}",
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
}

// ---------------------------------------------------------------------------
// Basic add / block detection
// ---------------------------------------------------------------------------

#[test]
fn adds_trailer_to_existing_block() {
    assert_same_stdin(
        &["--trailer", "Acked-by=B <b@example.com>"],
        b"subject line\n\nbody text here\n\nSigned-off-by: A <a@example.com>\n",
    );
}

#[test]
fn adds_trailer_when_no_block_present() {
    assert_same_stdin(
        &["--trailer", "Signed-off-by=A <a@example.com>"],
        b"subject line\n\nbody text here\n",
    );
}

#[test]
fn adds_trailer_to_subject_only_message() {
    assert_same_stdin(
        &["--trailer", "Signed-off-by=A <a@example.com>"],
        b"subject line\n",
    );
}

#[test]
fn multiline_subject_then_trailer() {
    assert_same_stdin(
        &["--trailer", "Reviewed-by=R <r@example.com>"],
        b"subject line one\nsubject line two\n\nbody\n",
    );
}

#[test]
fn message_missing_trailing_newline() {
    assert_same_stdin(
        &["--trailer", "Acked-by=Z"],
        b"subject line\n\nbody with no final newline",
    );
}

#[test]
fn trailing_blank_lines_are_preserved() {
    assert_same_stdin(&["--trailer", "Acked-by=Z"], b"subject\n\nbody\n\n\n");
}

#[test]
fn single_paragraph_trailer_lookalike_is_body() {
    // A lone paragraph that looks like trailers is the message body, so a new
    // trailer is appended as a fresh paragraph.
    assert_same_stdin(&["--trailer", "New=x"], b"Ack: 1\nRev: 2\n");
}

#[test]
fn non_trailer_line_breaks_block_without_prefix() {
    assert_same_stdin(
        &["--only-trailers"],
        b"subject\n\nthis is not a trailer\nAcked-by: A\n",
    );
}

#[test]
fn signed_off_by_prefix_enables_block_with_prose() {
    // A git-generated prefix lets a paragraph with <=75% prose still count.
    assert_same_stdin(
        &["--only-trailers"],
        b"subject\n\nSigned-off-by: A\nfollow up prose\nmore prose\nyet more\n",
    );
    assert_same_stdin(
        &["--only-trailers"],
        b"subject\n\nSigned-off-by: A\np1\np2\np3\np4\n",
    );
}

#[test]
fn continuation_lines_in_block() {
    assert_same_stdin(
        &["--only-trailers"],
        b"subject\n\nAcked-by: first\n continued value\nReviewed-by: second\n",
    );
}

// ---------------------------------------------------------------------------
// --only-trailers / --parse / --unfold / --only-input
// ---------------------------------------------------------------------------

#[test]
fn only_trailers_filters_body() {
    assert_same_stdin(
        &["--only-trailers"],
        b"subject\n\nbody\n\nAcked-by: A\nReviewed-by: B\n",
    );
}

#[test]
fn only_trailers_with_added_trailer() {
    assert_same_stdin(
        &["--only-trailers", "--trailer", "Cc=team@example.com"],
        b"subject\n\nbody\n\nAcked-by: A\n",
    );
}

#[test]
fn parse_alias() {
    assert_same_stdin(
        &["--parse"],
        b"subject\n\nAcked-by: A\n# a comment\nReviewed-by: B\n folded\n",
    );
}

#[test]
fn unfold_collapses_multiline_values() {
    assert_same_stdin(
        &["--only-trailers", "--unfold"],
        b"subject\n\nAcked-by: line one\n  line two\n\tline three\n",
    );
}

#[test]
fn unfold_preserves_internal_spacing() {
    assert_same_stdin(
        &["--only-trailers", "--unfold"],
        b"subject\n\nKey: value with trailing   \n  continuation\n",
    );
}

#[test]
fn only_input_leaves_block_untouched() {
    // `--only-input` parses and re-emits the input trailers without applying any
    // config; with no `--trailer` it is a normalising no-op on the block.
    assert_same_stdin(
        &["--only-input"],
        b"subject\n\nbody\n\nReviewed-by:   padded  \nAcked-by: B\n",
    );
}

#[test]
fn only_input_with_trailer_is_rejected() {
    // git refuses to queue trailers that `--only-input` would discard.
    assert_same_stdin(
        &["--only-input", "--trailer", "Acked-by=x"],
        b"subject\n\nbody\n\nReviewed-by: A\n",
    );
    // `--parse` implies `--only-input`, so the same rejection applies.
    assert_same_stdin(
        &["--parse", "--trailer", "Acked-by=x"],
        b"subject\n\nbody\n",
    );
}

// ---------------------------------------------------------------------------
// --trim-empty
// ---------------------------------------------------------------------------

#[test]
fn trim_empty_drops_empty_input_trailers() {
    assert_same_stdin(
        &["--trim-empty", "--only-trailers"],
        b"subject\n\nAcked-by:\nReviewed-by: B\n",
    );
}

#[test]
fn trim_empty_drops_empty_added_trailer() {
    assert_same_stdin(
        &["--trim-empty", "--trailer", "Acked-by="],
        b"subject\n\nbody\n\nReviewed-by: B\n",
    );
}

// ---------------------------------------------------------------------------
// --if-exists
// ---------------------------------------------------------------------------

#[test]
fn if_exists_variants() {
    for action in [
        "addIfDifferent",
        "addIfDifferentNeighbor",
        "replace",
        "doNothing",
        "add",
    ] {
        // Different value than the existing trailer.
        assert_same_stdin(
            &["--if-exists", action, "--trailer", "Acked-by=C"],
            b"subject\n\nbody\n\nAcked-by: B\n",
        );
        // Same value as the existing trailer.
        assert_same_stdin(
            &["--if-exists", action, "--trailer", "Acked-by=B"],
            b"subject\n\nbody\n\nAcked-by: B\n",
        );
        // Existing same-token trailer is not the last one (neighbor matters).
        assert_same_stdin(
            &["--if-exists", action, "--trailer", "Acked-by=B"],
            b"subject\n\nbody\n\nAcked-by: B\nReviewed-by: X\n",
        );
        // Two existing same-token trailers (replace removes only the matched).
        assert_same_stdin(
            &["--if-exists", action, "--trailer", "Acked-by=D"],
            b"subject\n\nbody\n\nAcked-by: B\nAcked-by: C\n",
        );
    }
}

// ---------------------------------------------------------------------------
// --if-missing
// ---------------------------------------------------------------------------

#[test]
fn if_missing_variants() {
    for action in ["add", "doNothing"] {
        assert_same_stdin(
            &["--if-missing", action, "--trailer", "Reviewed-by=C"],
            b"subject\n\nbody\n\nAcked-by: B\n",
        );
    }
}

// ---------------------------------------------------------------------------
// --where placement
// ---------------------------------------------------------------------------

#[test]
fn where_placement_variants() {
    for placement in ["after", "before", "end", "start"] {
        // Token that already exists.
        assert_same_stdin(
            &["--where", placement, "--trailer", "Acked-by=NEW"],
            b"subject\n\nbody\n\nAcked-by: B\nReviewed-by: X\n",
        );
        // Token that does not exist yet.
        assert_same_stdin(
            &["--where", placement, "--trailer", "Cc=NEW"],
            b"subject\n\nbody\n\nAcked-by: B\nReviewed-by: X\n",
        );
    }
}

#[test]
fn multiple_trailers_apply_in_order() {
    assert_same_stdin(
        &["--trailer", "Acked-by=1", "--trailer", "Acked-by=2"],
        b"subject\n\nbody\n",
    );
    assert_same_stdin(
        &["--trailer", "Acked-by=1", "--trailer", "Acked-by=2"],
        b"subject\n\nbody\n\nAcked-by: 0\n",
    );
    assert_same_stdin(
        &[
            "--trailer",
            "Acked-by=1",
            "--trailer",
            "Reviewed-by=2",
            "--trailer",
            "Acked-by=3",
        ],
        b"subject\n\nbody\n",
    );
}

#[test]
fn per_trailer_where_overrides() {
    // A `--where` only affects the trailers that follow it.
    assert_same_stdin(
        &[
            "--where",
            "after",
            "--trailer",
            "Acked-by=A",
            "--where",
            "before",
            "--trailer",
            "Reviewed-by=R",
        ],
        b"subject\n\nbody\n\nAcked-by: B\nReviewed-by: X\n",
    );
}

#[test]
fn token_prefix_matching() {
    // git compares tokens over the shorter length, so `Ack` matches `Acked-by`.
    assert_same_stdin(
        &["--if-exists", "replace", "--trailer", "Ack=z"],
        b"subject\n\nbody\n\nAcked-by: B\n",
    );
}

// ---------------------------------------------------------------------------
// Separators in --trailer arguments
// ---------------------------------------------------------------------------

#[test]
fn trailer_arg_separators() {
    assert_same_stdin(&["--only-trailers", "--trailer", "key=a:b"], b"s\n\nbody\n");
    assert_same_stdin(&["--only-trailers", "--trailer", "key:a=b"], b"s\n\nbody\n");
    assert_same_stdin(&["--only-trailers", "--trailer", "keyonly"], b"s\n\nbody\n");
    assert_same_stdin(
        &["--only-trailers", "--trailer", "key=   spaced value   "],
        b"s\n\nbody\n",
    );
    // Attached `--trailer=...` form.
    assert_same_stdin(&["--only-trailers", "--trailer=Cc=x"], b"s\n\nbody\n");
}

#[test]
fn output_separator_normalisation() {
    // Input uses `:`; values get whitespace-trimmed and an empty value renders
    // as `Token: `.
    assert_same_stdin(
        &["--only-trailers"],
        b"s\n\nKey :   padded value  \nEmpty:\nOk: fine\n",
    );
}

// ---------------------------------------------------------------------------
// Divider handling
// ---------------------------------------------------------------------------

#[test]
fn divider_preserves_patch() {
    assert_same_stdin(
        &["--trailer", "Acked-by=B"],
        b"subject\n\nbody\n\nReviewed-by: A\n---\ndiff --git a b\n+added\n",
    );
}

#[test]
fn divider_with_no_trailer_block_before_it() {
    assert_same_stdin(
        &["--trailer", "Acked-by=B"],
        b"subject\n\nbody\n---\ndiff text\n",
    );
}

#[test]
fn no_divider_treats_dashes_as_body() {
    assert_same_stdin(
        &["--no-divider", "--trailer", "Acked-by=B"],
        b"subject\n\nbody\n---\nmore text\n",
    );
}

#[test]
fn divider_only_when_followed_by_whitespace() {
    // `----` and `---x` are not dividers; `--- foo` is.
    assert_same_stdin(&["--trailer", "X=1"], b"subject\n\nbody\n----\nmore\n");
    assert_same_stdin(&["--trailer", "X=1"], b"subject\n\nbody\n---x\nmore\n");
    assert_same_stdin(&["--trailer", "X=1"], b"subject\n\nbody\n--- foo\nmore\n");
}

// ---------------------------------------------------------------------------
// Comments inside the trailer block
// ---------------------------------------------------------------------------

#[test]
fn interior_comment_is_dropped() {
    assert_same_stdin(
        &["--trailer", "Reviewed-by=R"],
        b"subject\n\nbody\n\nAcked-by: A\n# interior comment\nCc: c@example.com\n",
    );
}

#[test]
fn trailing_comment_is_kept() {
    assert_same_stdin(
        &["--trailer", "Reviewed-by=R"],
        b"subject\n\nbody\n\nAcked-by: A\n# trailing comment\n",
    );
}

// ---------------------------------------------------------------------------
// File operands and --in-place
// ---------------------------------------------------------------------------

#[test]
fn single_file_operand() {
    if !git_available() {
        return;
    }
    let dir = unique_temp_dir("interpret-trailers-file");
    fs::write(dir.join("msg.txt"), b"subject\n\nbody\n").expect("write fixture");
    assert_same_args(&dir, &["--trailer", "Acked-by=A", "msg.txt"]);
}

#[test]
fn multiple_file_operands_concatenate() {
    if !git_available() {
        return;
    }
    let dir = unique_temp_dir("interpret-trailers-files");
    fs::write(dir.join("a.txt"), b"subj a\n\nbody a\n").expect("write a");
    fs::write(dir.join("b.txt"), b"subj b\n\nbody b\n").expect("write b");
    assert_same_args(&dir, &["--trailer", "Acked-by=A", "a.txt", "b.txt"]);
}

#[test]
fn double_dash_then_file() {
    if !git_available() {
        return;
    }
    let dir = unique_temp_dir("interpret-trailers-ddash");
    fs::write(dir.join("m.txt"), b"subject\n\nbody\n").expect("write fixture");
    assert_same_args(&dir, &["--trailer", "Acked-by=A", "--", "m.txt"]);
}

#[test]
fn in_place_rewrites_file_identically() {
    if !git_available() {
        return;
    }
    let content = b"subject\n\nbody\n\nReviewed-by: A\n";
    // Run git and sley in separate dirs on identical inputs, then compare the
    // rewritten files plus the (empty) stdout/stderr/exit.
    let git_dir = unique_temp_dir("interpret-trailers-inplace-git");
    let rs_dir = unique_temp_dir("interpret-trailers-inplace-rs");
    fs::write(git_dir.join("m.txt"), content).expect("write git fixture");
    fs::write(rs_dir.join("m.txt"), content).expect("write rs fixture");

    let git_out = run(
        sley_testkit::oracle_git(),
        &git_dir,
        &[
            "interpret-trailers",
            "--in-place",
            "--trailer",
            "Acked-by=B",
            "m.txt",
        ],
    );
    let rs_out = run(
        git_rs_bin(),
        &rs_dir,
        &[
            "interpret-trailers",
            "--in-place",
            "--trailer",
            "Acked-by=B",
            "m.txt",
        ],
    );

    assert_eq!(
        git_out.status.code(),
        rs_out.status.code(),
        "exit code differed"
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_out.stdout),
        String::from_utf8_lossy(&git_out.stdout),
        "stdout differed",
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_out.stderr),
        String::from_utf8_lossy(&git_out.stderr),
        "stderr differed",
    );
    let git_file = fs::read(git_dir.join("m.txt")).expect("read git result");
    let rs_file = fs::read(rs_dir.join("m.txt")).expect("read rs result");
    assert_eq!(
        String::from_utf8_lossy(&rs_file),
        String::from_utf8_lossy(&git_file),
        "in-place file contents differed",
    );
}

#[test]
fn missing_file_operand_is_fatal() {
    if !git_available() {
        return;
    }
    let dir = unique_temp_dir("interpret-trailers-missing");
    assert_same_args(&dir, &["--trailer", "Acked-by=A", "definitely-missing.txt"]);
}

// ---------------------------------------------------------------------------
// Help and option errors
// ---------------------------------------------------------------------------

#[test]
fn help_text_matches() {
    // `-h` prints the short usage to stdout and exits 129. (`--help` is excluded:
    // real git execs the man page, which is not reproducible in a hermetic test.)
    let cwd = std::env::temp_dir();
    assert_same_args(&cwd, &["-h"]);
}

#[test]
fn unknown_option_matches() {
    let cwd = std::env::temp_dir();
    // Long options are reported as an unknown "option"...
    assert_same_args(&cwd, &["--definitely-not-an-option"]);
    // ...short ones as an unknown "switch".
    assert_same_args(&cwd, &["-x"]);
}

#[test]
fn invalid_enum_values_exit_129() {
    assert_same_stdin(&["--where", "sideways", "--trailer", "A=1"], b"s\n\nb\n");
    assert_same_stdin(&["--if-exists", "maybe", "--trailer", "A=1"], b"s\n\nb\n");
    assert_same_stdin(
        &["--if-missing", "perhaps", "--trailer", "A=1"],
        b"s\n\nb\n",
    );
}

#[test]
fn missing_option_value_matches() {
    assert_same_stdin(&["--trailer"], b"s\n\nb\n");
    assert_same_stdin(&["--where"], b"s\n\nb\n");
}

// ---------------------------------------------------------------------------
// No-op normalisation
// ---------------------------------------------------------------------------

#[test]
fn no_args_normalises_message() {
    assert_same_stdin(&[], b"subject\n\nbody\n\nAcked-by: A\n");
    assert_same_stdin(&[], b"subject\n\nbody line\n");
    assert_same_stdin(&[], b"subject\n");
}
