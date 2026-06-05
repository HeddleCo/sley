//! Differential interop tests for `git notes`: every assertion compares
//! sley output (status + stdout + stderr) against the system `git` binary
//! running the same command in an equivalent temp repository, and also checks
//! that the resulting notes objects (commit, tree, blob) are byte-identical.
//!
//! The whole suite is skipped when no usable system `git` is present.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed identity/date so commit and notes objects hash deterministically and
/// match between the sley and system-git repositories.
const NAME: &str = "Tester";
const EMAIL: &str = "tester@example.com";
const DATE: &str = "@1790000000 -0500";

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

/// True when a system `git` is available; the suite no-ops otherwise.
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", NAME)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_NAME", NAME)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_COMMITTER_DATE", DATE)
        // Keep the environment clean of any inherited notes ref selection.
        .env_remove("GIT_NOTES_REF")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

/// Run with extra environment entries (used to exercise GIT_NOTES_REF).
fn run_output_env(program: &str, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", NAME)
        .env("GIT_AUTHOR_EMAIL", EMAIL)
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_NAME", NAME)
        .env("GIT_COMMITTER_EMAIL", EMAIL)
        .env("GIT_COMMITTER_DATE", DATE)
        .env_remove("GIT_NOTES_REF");
    for (key, value) in extra {
        command.env(key, value);
    }
    command
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_ok(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_ok("git", cwd, args)
}

fn git_rs_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn assert_same_output(actual: &Output, expected: &Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}\nsley stderr:\n{}\ngit stderr:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
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

/// Initialize a repo with `count` commits ("c1", "c2", ...) and a worktree
/// file per commit, using deterministic identity/date.
fn init_repo_with_commits(root: &Path, count: usize) {
    run_ok("git", root, &["init", "-q"]);
    for i in 1..=count {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("content {i}\n")).expect("write worktree file");
        git(root, &["add", name.as_str()]);
        let message = format!("c{i}");
        git(root, &["commit", "-q", "-m", message.as_str()]);
    }
}

/// Mirror of `init_repo_with_commits` into two sibling repos so the same
/// sequence of mutating commands can be replayed against git and sley and the
/// objects compared.
fn make_repo_pair(root: &Path, label: &str, commits: usize) -> (PathBuf, PathBuf) {
    let expected = root.join(format!("{label}-expected"));
    let actual = root.join(format!("{label}-actual"));
    std::fs::create_dir_all(&expected).expect("create expected repo");
    std::fs::create_dir_all(&actual).expect("create actual repo");
    init_repo_with_commits(&expected, commits);
    init_repo_with_commits(&actual, commits);
    (expected, actual)
}

/// Run the same notes command in both repos and assert identical CLI output.
/// The system git runs in the "-expected" repo and sley in the "-actual"
/// repo, so each tool mutates its own copy.
fn assert_notes_match(expected_root: &Path, actual_root: &Path, args: &[&str]) {
    let expected = run_output("git", expected_root, args);
    let actual = run_output(git_rs_bin(), actual_root, args);
    assert_same_output(&actual, &expected, args);
}

/// Assert that a notes ref resolves to byte-identical commit/tree content in
/// both repos (so the on-disk object graph sley produced matches git's).
fn assert_notes_object_match(expected_root: &Path, actual_root: &Path, notes_ref: &str) {
    let expected = run_output("git", expected_root, &["rev-parse", notes_ref]);
    let actual = run_output(git_rs_bin(), actual_root, &["rev-parse", notes_ref]);
    assert_same_output(&actual, &expected, &["rev-parse", notes_ref]);
    if !expected.status.success() {
        return;
    }
    // The commit oids being equal already proves tree+blob+message equality,
    // but compare the pretty-printed commit and tree explicitly for a clearer
    // failure if hashing ever diverges.
    for spec in [notes_ref.to_string(), format!("{notes_ref}^{{tree}}")] {
        let e = git(expected_root, &["cat-file", "-p", spec.as_str()]);
        let a = git(actual_root, &["cat-file", "-p", spec.as_str()]);
        assert_eq!(
            String::from_utf8_lossy(&a),
            String::from_utf8_lossy(&e),
            "notes object content differed for {spec}"
        );
    }
}

#[test]
fn notes_add_show_list_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-add-show-list");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "basic", 3);

        // add with -m, then show / list (all) / list <object>.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "add", "-m", "first note", "HEAD"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD"]);
        // Default object is HEAD.
        assert_notes_match(&expected, &actual, &["notes", "show"]);
        assert_notes_match(&expected, &actual, &["notes", "list", "HEAD"]);

        // Multiple -m paragraphs, on a different commit.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "add", "-m", "line one", "-m", "line two", "HEAD~1"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD~1"]);

        // list (all) and the no-subcommand alias both dump every note.
        assert_notes_match(&expected, &actual, &["notes", "list"]);
        assert_notes_match(&expected, &actual, &["notes"]);
        assert_notes_match(&expected, &actual, &["notes", "get-ref"]);
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_add_show_list_match_git assertions");
}

#[test]
fn notes_overwrite_and_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-overwrite-errors");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "ow", 2);

        assert_notes_match(&expected, &actual, &["notes", "add", "-m", "orig", "HEAD"]);
        // Re-adding without -f is an error on both.
        assert_notes_match(&expected, &actual, &["notes", "add", "-m", "again", "HEAD"]);
        // -f prints the "Overwriting" line and succeeds.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "add", "-f", "-m", "replaced", "HEAD"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD"]);

        // show / list on an object that has no note.
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD~1"]);
        assert_notes_match(&expected, &actual, &["notes", "list", "HEAD~1"]);

        // Unresolvable object.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "add", "-m", "x", "no-such-rev"],
        );
        assert_notes_match(&expected, &actual, &["notes", "show", "no-such-rev"]);
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_overwrite_and_errors_match_git assertions");
}

#[test]
fn notes_append_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-append");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "ap", 2);

        // append onto a fresh object creates the note.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "append", "-m", "first line", "HEAD"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        // append onto an existing note adds a blank-line-separated paragraph.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "append", "-m", "second line", "HEAD"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD"]);
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_append_matches_git assertions");
}

#[test]
fn notes_remove_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-remove");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "rm", 3);

        git(&expected, &["notes", "add", "-m", "a", "HEAD"]);
        run_ok(git_rs_bin(), &actual, &["notes", "add", "-m", "a", "HEAD"]);
        git(&expected, &["notes", "add", "-m", "b", "HEAD~1"]);
        run_ok(
            git_rs_bin(),
            &actual,
            &["notes", "add", "-m", "b", "HEAD~1"],
        );

        // remove a present note (echoes the literal spec), then a missing one.
        assert_notes_match(&expected, &actual, &["notes", "remove", "HEAD"]);
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "remove", "HEAD"]);
        // --ignore-missing turns the missing case into a success.
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "remove", "--ignore-missing", "HEAD"],
        );

        // Remove the last remaining note: ref must advance to an empty-tree
        // commit in both implementations (not be deleted).
        assert_notes_match(&expected, &actual, &["notes", "remove", "HEAD~1"]);
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "list"]);
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_remove_matches_git assertions");
}

#[test]
fn notes_copy_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-copy");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "cp", 3);

        git(&expected, &["notes", "add", "-m", "source note", "HEAD"]);
        run_ok(
            git_rs_bin(),
            &actual,
            &["notes", "add", "-m", "source note", "HEAD"],
        );

        // copy to a note-less object.
        assert_notes_match(&expected, &actual, &["notes", "copy", "HEAD", "HEAD~2"]);
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
        assert_notes_match(&expected, &actual, &["notes", "show", "HEAD~2"]);

        // copy onto an object that already has a note: error without -f, ok with.
        git(&expected, &["notes", "add", "-m", "dest note", "HEAD~1"]);
        run_ok(
            git_rs_bin(),
            &actual,
            &["notes", "add", "-m", "dest note", "HEAD~1"],
        );
        assert_notes_match(&expected, &actual, &["notes", "copy", "HEAD", "HEAD~1"]);
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "copy", "-f", "HEAD", "HEAD~1"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");

        // copy from an object with no note (HEAD has no note on the dest path
        // here is irrelevant; source HEAD~2 currently has the copied note, so
        // this exercises the from-has-note path with -f overwrite).
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "copy", "-f", "HEAD~2", "HEAD~1"],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/commits");
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_copy_matches_git assertions");
}

#[test]
fn notes_custom_ref_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-custom-ref");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "ref", 2);

        // --ref shorthand lands under refs/notes/.
        assert_notes_match(
            &expected,
            &actual,
            &[
                "notes",
                "--ref",
                "review",
                "add",
                "-m",
                "review note",
                "HEAD",
            ],
        );
        assert_notes_object_match(&expected, &actual, "refs/notes/review");
        assert_notes_match(
            &expected,
            &actual,
            &["notes", "--ref", "review", "show", "HEAD"],
        );
        assert_notes_match(&expected, &actual, &["notes", "--ref=review", "get-ref"]);
        // The default ref is untouched.
        assert_notes_match(&expected, &actual, &["notes", "list"]);

        // GIT_NOTES_REF selects the ref when no --ref is given.
        let env = [("GIT_NOTES_REF", "refs/notes/review")];
        let expected_out = run_output_env("git", &expected, &["notes", "get-ref"], &env);
        let actual_out = run_output_env(git_rs_bin(), &actual, &["notes", "get-ref"], &env);
        assert_same_output(&actual_out, &expected_out, &["notes", "get-ref"]);
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_custom_ref_matches_git assertions");
}

#[test]
fn notes_usage_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("notes-usage-errors");
    std::fs::create_dir_all(&root).expect("create temp root");
    let result = std::panic::catch_unwind(|| {
        let (expected, actual) = make_repo_pair(&root, "usage", 1);

        for args in [
            vec!["notes", "bogus-subcommand"],
            vec!["notes", "--bogus"],
            vec!["notes", "-x"],
            vec!["notes", "add", "--bogus"],
            vec!["notes", "add", "-z"],
            vec!["notes", "add", "-m"],
            vec!["notes", "add", "-F"],
            vec!["notes", "add", "-C"],
            vec!["notes", "show", "--bogus"],
            vec!["notes", "show", "a", "b"],
            vec!["notes", "list", "a", "b"],
            vec!["notes", "remove", "--bogus"],
            vec!["notes", "copy"],
            vec!["notes", "copy", "a", "b", "c"],
            vec!["notes", "get-ref", "extra"],
            vec!["notes", "--ref"],
        ] {
            assert_notes_match(&expected, &actual, &args);
        }
    });
    let _ = std::fs::remove_dir_all(&root);
    result.expect("notes_usage_errors_match_git assertions");
}
