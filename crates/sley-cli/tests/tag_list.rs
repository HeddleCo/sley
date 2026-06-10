use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity_and_editor(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .env("GIT_EDITOR", "true")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_named_identity_at(
    program: &str,
    cwd: &Path,
    args: &[&str],
    timestamp: i64,
    name: &str,
    email: &str,
) -> Output {
    let date = format!("@{timestamp} +0000");
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_identity_at(program: &str, cwd: &Path, args: &[&str], timestamp: i64) -> Vec<u8> {
    run_with_named_identity_at(
        program,
        cwd,
        args,
        timestamp,
        "Example User",
        "example@example.invalid",
    )
}

fn run_with_named_identity_at(
    program: &str,
    cwd: &Path,
    args: &[&str],
    timestamp: i64,
    name: &str,
    email: &str,
) -> Vec<u8> {
    let date = format!("@{timestamp} +0000");
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
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
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn prepare_tag_message_repo(root: &Path) {
    run(sley_testkit::oracle_git(), root, &["init", "-q", "-b", "main"]);
    let commit = run_output_with_identity(
        sley_testkit::oracle_git(),
        root,
        &["commit", "--allow-empty", "-q", "-m", "initial"],
    );
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    fs::write(root.join("message-no-lf.txt"), b"file one").expect("write no-lf message");
    fs::write(root.join("message-lf.txt"), b"file two\n").expect("write lf message");
}

fn cat_tag(program: &str, root: &Path, tag: &str) -> Vec<u8> {
    let output = run_output(program, root, &["cat-file", "-p", tag]);
    assert!(
        output.status.success(),
        "{program} cat-file failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn read_tag_reflog(root: &Path, tag: &str) -> Option<Vec<u8>> {
    fs::read(
        root.join(".git")
            .join("logs")
            .join("refs")
            .join("tags")
            .join(tag),
    )
    .ok()
}

#[test]
fn tag_create_errors_match_upstream_git() {
    let root = unique_temp_dir("tag-create-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Second User",
                "-c",
                "user.email=second@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-m",
                "initial body",
                "-q",
            ],
            1000,
        );
        git(&root, &["tag", "v1"]);

        for args in [
            vec!["tag", "v1"],
            vec!["tag", "-a", "v1", "-m", "annotated"],
            vec!["tag", "-m"],
            vec!["tag", "--message"],
            vec!["tag", "-F"],
            vec!["tag", "--file"],
            vec![
                "tag",
                "-a",
                "bad-no-annotate",
                "-m",
                "msg",
                "--no-annotate=foo",
            ],
            vec!["tag", "-a", "bad-no-sign", "-m", "msg", "--no-sign=foo"],
            vec![
                "tag",
                "-a",
                "bad-no-local-user",
                "-m",
                "msg",
                "--no-local-user=foo",
            ],
            vec!["tag", "bad-no-force", "--no-force=foo"],
            vec!["tag", "bad-force", "--force=foo"],
            vec!["tag", "bad-no-create-reflog", "--no-create-reflog=foo"],
            vec!["tag", "bad-create-reflog", "--create-reflog=foo"],
            vec!["tag", "-a", "bad-no-file", "-m", "msg", "--no-file=foo"],
            vec![
                "tag",
                "-a",
                "bad-no-cleanup",
                "-m",
                "msg",
                "--no-cleanup=foo",
            ],
            vec!["tag", "-a", "bad-no-edit", "-m", "msg", "--no-edit=foo"],
            vec!["tag", "-a", "bad-edit", "-m", "msg", "--edit=foo"],
            vec!["tag", "-a", "bad-annotate", "-m", "msg", "--annotate=foo"],
            vec!["tag", "bad-sign", "--sign=foo"],
            vec!["tag", "--verify=foo", "v1"],
            vec!["tag", "--no-delete"],
            vec!["tag", "--no-verify"],
            vec!["tag", "--no-list"],
            vec!["tag", "--no-delete="],
            vec!["tag", "--no-verify="],
            vec!["tag", "--no-list="],
            vec!["tag", "-a=foo"],
            vec!["tag", "-s=foo"],
            vec!["tag", "-f=foo"],
            vec!["tag", "-d=foo"],
            vec!["tag", "-v=foo"],
            vec!["tag", "-l=foo"],
            vec!["tag", "-e=foo"],
            vec!["tag", "-i=foo"],
            vec!["tag", "-n=foo"],
            vec!["tag", "-n1kb"],
            vec!["tag", "-n999999999999999999999999999999999999999g"],
            vec!["tag", "--unknown"],
            vec!["tag", "--unknown=value"],
            vec!["tag", "--foo", "v1"],
            vec!["tag", "-x"],
            vec!["tag", "-xfoo"],
            vec!["tag", "-bad"],
            vec!["tag", "-ab"],
            vec!["tag", "-Z=value"],
            vec!["tag", "-av"],
            vec!["tag", "-fl"],
            vec!["tag", "-ai"],
            vec!["tag", "-vf"],
            vec!["tag", "--points-at", "bogus"],
            vec!["tag", "--points-at", "--no-points-at"],
            vec!["tag", "--points-at="],
            vec!["tag", "--contains", "bogus"],
            vec!["tag", "--contains", "--no-contains"],
            vec!["tag", "--contains="],
            vec!["tag", "--no-contains", "bogus"],
            vec!["tag", "--no-contains="],
            vec!["tag", "--merged", "bogus"],
            vec!["tag", "--merged", "--no-merged"],
            vec!["tag", "--merged="],
            vec!["tag", "--no-merged", "bogus"],
            vec!["tag", "--no-merged", "--merged"],
            vec!["tag", "--no-merged="],
            vec!["tag", "--sort="],
            vec!["tag", "--sort=bogus"],
            vec!["tag", "--sort=-bogus"],
            vec!["tag", "--file="],
            vec!["tag", "--delete", "--no-delete", "missing"],
            vec!["tag", "--verify", "--no-verify", "missing"],
            vec!["tag", "--list", "--no-list"],
            vec!["tag", "--", "--list"],
            vec!["tag", "--", "-bad"],
            vec!["tag", "--", "bad/name/"],
            vec!["tag", "--", "bad", "missing"],
            vec!["tag", "too", "many", "arguments"],
            vec!["tag", "-a", "too", "many", "arguments", "-m", "msg"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_file_messages_match_upstream_git_objects() {
    let root = unique_temp_dir("tag-file-messages");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for (name, args) in [
            (
                "file-no-lf",
                vec!["tag", "-F", "message-no-lf.txt", "file-no-lf"],
            ),
            (
                "attached-file",
                vec!["tag", "-Fmessage-lf.txt", "attached-file"],
            ),
            (
                "long-file-equals",
                vec!["tag", "--file=message-no-lf.txt", "long-file-equals"],
            ),
            (
                "empty-file-equals",
                vec!["tag", "--file=", "empty-file-equals"],
            ),
            (
                "empty-file-after-message",
                vec![
                    "tag",
                    "--message",
                    "inline",
                    "--file=",
                    "empty-file-after-message",
                ],
            ),
            (
                "long-file",
                vec!["tag", "--file", "message-lf.txt", "long-file"],
            ),
            (
                "file-last-wins",
                vec![
                    "tag",
                    "--file",
                    "message-no-lf.txt",
                    "-Fmessage-lf.txt",
                    "file-last-wins",
                ],
            ),
            (
                "file-noop",
                vec![
                    "tag",
                    "--file",
                    "message-no-lf.txt",
                    "--no-file",
                    "file-noop",
                ],
            ),
            (
                "attached-message",
                vec!["tag", "-mattached", "attached-message"],
            ),
            (
                "long-message",
                vec!["tag", "--message", "inline", "long-message"],
            ),
            (
                "long-message-equals",
                vec!["tag", "--message=inline", "long-message-equals"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                cat_tag(sley_testkit::oracle_git(), &actual_root, name),
                cat_tag(sley_testkit::oracle_git(), &expected_root, name),
                "tag object differed for {args:?}"
            );
        }

        let expected_root = root.join("mixed-message-expected");
        let actual_root = root.join("mixed-message-actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_tag_message_repo(&expected_root);
        prepare_tag_message_repo(&actual_root);
        let args = ["tag", "-F", "message-no-lf.txt", "-m", "inline", "mixed"];
        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_delete_missing_matches_upstream_git() {
    let root = unique_temp_dir("tag-delete-missing");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-m",
                "initial body",
                "-q",
            ],
            1000,
        );
        git(&root, &["tag", "v1"]);

        for args in [
            vec!["tag", "-d", "missing"],
            vec!["tag", "--delete", "missing"],
            vec!["tag", "-d"],
            vec!["tag", "-d", "--", "-bad"],
            vec!["tag", "-d", "--", "--list"],
            vec!["tag", "-d", "--", "bad/name/"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        let args = ["tag", "-d", "v1", "missing"];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "v1"]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_cleanup_modes_match_upstream_git_objects() {
    let root = unique_temp_dir("tag-cleanup-modes");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for (name, args) in [
            (
                "strip-equals",
                vec![
                    "tag",
                    "-a",
                    "strip-equals",
                    "--cleanup=strip",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
            (
                "strip-space",
                vec![
                    "tag",
                    "-a",
                    "strip-space",
                    "--cleanup",
                    "strip",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
            (
                "whitespace",
                vec![
                    "tag",
                    "-a",
                    "whitespace",
                    "--cleanup=whitespace",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
            (
                "verbatim",
                vec![
                    "tag",
                    "-a",
                    "verbatim",
                    "--cleanup=verbatim",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
            (
                "file-verbatim",
                vec![
                    "tag",
                    "-a",
                    "file-verbatim",
                    "--cleanup=verbatim",
                    "-F",
                    "message-no-lf.txt",
                ],
            ),
            (
                "no-cleanup",
                vec![
                    "tag",
                    "-a",
                    "no-cleanup",
                    "--cleanup=verbatim",
                    "--no-cleanup",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
            (
                "no-edit",
                vec![
                    "tag",
                    "-a",
                    "no-edit",
                    "--no-edit",
                    "-m",
                    " subject ",
                    "-m",
                    "#comment",
                    "-m",
                    "body  ",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                cat_tag(sley_testkit::oracle_git(), &actual_root, name),
                cat_tag(sley_testkit::oracle_git(), &expected_root, name),
                "tag object differed for {args:?}"
            );
        }

        for (name, args) in [
            (
                "cleanup-missing",
                vec!["tag", "-a", "cleanup-missing", "--cleanup"],
            ),
            (
                "cleanup-bad",
                vec!["tag", "-a", "cleanup-bad", "--cleanup=bad", "-m", "message"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_trailers_match_upstream_git_objects() {
    let root = unique_temp_dir("tag-trailers");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for (name, args) in [
            (
                "trailer-equals",
                vec![
                    "tag",
                    "-a",
                    "trailer-equals",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "trailer-colon",
                vec![
                    "tag",
                    "-a",
                    "trailer-colon",
                    "-m",
                    "subject",
                    "--trailer=Acked-by:Alice",
                ],
            ),
            (
                "trailer-multiple",
                vec![
                    "tag",
                    "-a",
                    "trailer-multiple",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                    "--trailer",
                    "Reviewed-by=Bob",
                ],
            ),
            (
                "trailer-body",
                vec![
                    "tag",
                    "-a",
                    "trailer-body",
                    "-m",
                    "subject",
                    "-m",
                    "body",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "trailer-existing",
                vec![
                    "tag",
                    "-a",
                    "trailer-existing",
                    "-m",
                    "subject",
                    "-m",
                    "Existing: One",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "trailer-empty-value",
                vec![
                    "tag",
                    "-a",
                    "trailer-empty-value",
                    "-m",
                    "subject",
                    "--trailer",
                    "BadTrailer",
                ],
            ),
            (
                "trailer-clear",
                vec![
                    "tag",
                    "-a",
                    "trailer-clear",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                    "--no-trailer",
                ],
            ),
            (
                "trailer-clear-before",
                vec![
                    "tag",
                    "-a",
                    "trailer-clear-before",
                    "-m",
                    "subject",
                    "--no-trailer",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "trailer-clear-middle",
                vec![
                    "tag",
                    "-a",
                    "trailer-clear-middle",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                    "--no-trailer",
                    "--trailer",
                    "Reviewed-by=Bob",
                ],
            ),
            (
                "trailer-only",
                vec!["tag", "-a", "trailer-only", "--trailer", "Acked-by=Alice"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = if name == "trailer-only" {
                run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args)
            } else {
                run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args)
            };
            let actual = if name == "trailer-only" {
                run_output_with_identity_and_editor(env!("CARGO_BIN_EXE_sley"), &actual_root, &args)
            } else {
                run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args)
            };
            assert_same_output(actual, expected, &args);
            assert_eq!(
                cat_tag(sley_testkit::oracle_git(), &actual_root, name),
                cat_tag(sley_testkit::oracle_git(), &expected_root, name),
                "tag object differed for {args:?}"
            );
        }

        let expected_root = root.join("missing-expected");
        let actual_root = root.join("missing-actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_tag_message_repo(&expected_root);
        prepare_tag_message_repo(&actual_root);
        for args in [
            vec!["tag", "-a", "missing", "--trailer"],
            vec![
                "tag",
                "-a",
                "bad-no-trailer",
                "-m",
                "subject",
                "--no-trailer=foo",
            ],
        ] {
            let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_force_create_and_update_match_upstream_git() {
    let root = unique_temp_dir("tag-force");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
            1000,
        );
        let first_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("first HEAD oid is utf8")
            .trim()
            .to_string();
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "second",
                "-q",
            ],
            2000,
        );
        let second_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("second HEAD oid is utf8")
            .trim()
            .to_string();

        let args = ["tag", "--force", "new-force", first_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "-d", "new-force"]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "new-force"]),
            format!("{first_oid}\n").into_bytes()
        );

        let args = ["tag", "--", "separator", first_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "-d", "separator"]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "separator"]),
            format!("{first_oid}\n").into_bytes()
        );

        git(&root, &["tag", "-f", "v1", first_oid.as_str()]);
        let args = ["tag", "-f", "v1", second_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "-f", "v1", first_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "v1"]),
            format!("{second_oid}\n").into_bytes()
        );

        let args = ["tag", "-f", "--no-force", "v1", second_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);

        let args = ["tag", "--no-force", "-f", "v1", second_oid.as_str()];
        git(&root, &["tag", "-f", "v1", first_oid.as_str()]);
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "-f", "v1", first_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "v1"]),
            format!("{second_oid}\n").into_bytes()
        );

        git(&root, &["tag", "-f", "ann", first_oid.as_str()]);
        let args = [
            "tag",
            "-f",
            "-a",
            "ann",
            "-m",
            "annotated",
            second_oid.as_str(),
        ];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        git(&root, &["tag", "-f", "ann", first_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(git(&root, &["cat-file", "-t", "ann"]), b"tag\n");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_no_edit_lightweight_matches_upstream_git() {
    let root = unique_temp_dir("tag-no-edit-lightweight");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let expected_root = root.join("expected");
        let actual_root = root.join("actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_tag_message_repo(&expected_root);
        prepare_tag_message_repo(&actual_root);

        let args = ["tag", "no-edit-lightweight", "--no-edit"];
        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&actual_root, &["rev-parse", "no-edit-lightweight"]),
            git(&expected_root, &["rev-parse", "no-edit-lightweight"]),
            "tag target differed for {args:?}"
        );
        assert_eq!(
            git(&actual_root, &["cat-file", "-t", "no-edit-lightweight"]),
            git(&expected_root, &["cat-file", "-t", "no-edit-lightweight"]),
            "tag object type differed for {args:?}"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_edit_option_matches_upstream_git_objects() {
    let root = unique_temp_dir("tag-edit-option");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for (name, args) in [
            (
                "edit-long",
                vec!["tag", "--edit", "-m", "message", "edit-long"],
            ),
            (
                "edit-short",
                vec!["tag", "-e", "-m", "message", "edit-short"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_sley"),
                &actual_root,
                &args,
            );
            assert_same_output(actual, expected, &args);
            assert_eq!(
                cat_tag(sley_testkit::oracle_git(), &actual_root, name),
                cat_tag(sley_testkit::oracle_git(), &expected_root, name),
                "tag object differed for {args:?}"
            );
        }

        for args in [
            vec!["tag", "--edit", "missing-message"],
            vec!["tag", "-e", "missing-message"],
            vec!["tag", "-a", "missing-message"],
            vec!["tag", "--annotate", "missing-message"],
            vec!["tag", "-a", "--no-edit", "missing-message"],
        ] {
            let expected_root = root.join(format!("{}-expected", args[1]));
            let actual_root = root.join(format!("{}-actual", args[1]));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_sley"),
                &actual_root,
                &args,
            );
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_create_reflog_matches_upstream_git() {
    let root = unique_temp_dir("tag-create-reflog");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let expected_root = root.join("expected");
        let actual_root = root.join("actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        for repo in [&expected_root, &actual_root] {
            git(repo, &["init", "-q", "-b", "main"]);
            run_with_named_identity_at(
                sley_testkit::oracle_git(),
                repo,
                &["commit", "--allow-empty", "-m", "first subject", "-q"],
                86_400,
                "First User",
                "first@example.invalid",
            );
            run_with_named_identity_at(
                sley_testkit::oracle_git(),
                repo,
                &["commit", "--allow-empty", "-m", "Second Subject", "-q"],
                172_800,
                "Second User",
                "second@example.invalid",
            );
        }

        for (timestamp, args) in [
            (
                200_000,
                vec!["tag", "--create-reflog", "light-reflog", "HEAD~1"],
            ),
            (
                210_000,
                vec![
                    "tag",
                    "--create-reflog",
                    "--no-create-reflog",
                    "no-reflog",
                    "HEAD~1",
                ],
            ),
            (
                220_000,
                vec![
                    "tag",
                    "--no-create-reflog",
                    "--create-reflog",
                    "yes-reflog",
                    "HEAD~1",
                ],
            ),
            (
                230_000,
                vec!["tag", "--create-reflog", "force-reflog", "HEAD~1"],
            ),
            (
                240_000,
                vec!["tag", "-f", "--create-reflog", "force-reflog", "HEAD"],
            ),
            (
                250_000,
                vec![
                    "tag",
                    "-a",
                    "--create-reflog",
                    "ann-reflog",
                    "-m",
                    "annotated",
                    "HEAD~1",
                ],
            ),
        ] {
            let expected = run_output_with_named_identity_at(
                sley_testkit::oracle_git(),
                &expected_root,
                &args,
                timestamp,
                "Tag User",
                "tag@example.invalid",
            );
            let actual = run_output_with_named_identity_at(
                env!("CARGO_BIN_EXE_sley"),
                &actual_root,
                &args,
                timestamp,
                "Tag User",
                "tag@example.invalid",
            );
            assert_same_output(actual, expected, &args);
        }

        for tag in [
            "light-reflog",
            "no-reflog",
            "yes-reflog",
            "force-reflog",
            "ann-reflog",
        ] {
            assert_eq!(
                read_tag_reflog(&actual_root, tag),
                read_tag_reflog(&expected_root, tag),
                "reflog differed for {tag}"
            );
        }
        assert_eq!(
            git(&actual_root, &["cat-file", "-p", "ann-reflog"]),
            git(&expected_root, &["cat-file", "-p", "ann-reflog"]),
            "annotated tag object differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_annotate_negation_matches_upstream_git() {
    let root = unique_temp_dir("tag-annotate-negation");
    fs::create_dir_all(&root).expect("create temp root");
    for (name, args) in [
        ("plain", vec!["tag", "--annotate", "--no-annotate", "plain"]),
        (
            "ann",
            vec!["tag", "--no-annotate", "--annotate", "ann", "-m", "msg"],
        ),
        ("msg", vec!["tag", "--no-annotate", "msg", "-m", "message"]),
        ("nosign", vec!["tag", "--no-sign", "nosign"]),
        (
            "sign-cancelled",
            vec![
                "tag",
                "--sign",
                "--no-sign",
                "sign-cancelled",
                "-m",
                "message",
            ],
        ),
    ] {
        let expected_root = root.join(format!("{name}-expected"));
        let actual_root = root.join(format!("{name}-actual"));
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_tag_message_repo(&expected_root);
        prepare_tag_message_repo(&actual_root);

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&actual_root, &["cat-file", "-t", name]),
            git(&expected_root, &["cat-file", "-t", name]),
            "tag object type differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_verify_unsigned_and_lightweight_match_upstream_git() {
    let root = unique_temp_dir("tag-verify");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "--allow-empty", "-q", "-m", "initial"],
            1000,
        );
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &["tag", "-a", "ann", "-m", "annotated message"],
            2000,
        );
        git(&root, &["tag", "lw"]);

        for args in [
            vec!["tag", "-v"],
            vec!["tag", "-v", "ann"],
            vec!["tag", "--verify", "ann"],
            vec!["tag", "-v", "lw"],
            vec!["tag", "-v", "missing"],
            vec!["tag", "-v", "--", "-bad"],
            vec!["tag", "-v", "ann", "lw"],
            vec!["tag", "-v", "--format=%(refname)", "ann"],
            vec!["tag", "-v", "-l", "ann"],
            vec!["tag", "-v", "-n", "ann"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_local_user_negation_and_missing_values_match_upstream_git() {
    let root = unique_temp_dir("tag-local-user");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        for (name, args) in [
            ("lightweight", vec!["tag", "--no-local-user", "lightweight"]),
            (
                "annotated",
                vec!["tag", "--no-local-user", "-a", "annotated", "-m", "msg"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_tag_message_repo(&expected_root);
            prepare_tag_message_repo(&actual_root);

            let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                git(&actual_root, &["cat-file", "-t", name]),
                git(&expected_root, &["cat-file", "-t", name]),
                "tag object type differed for {args:?}"
            );
            if name == "annotated" {
                assert_eq!(
                    cat_tag(env!("CARGO_BIN_EXE_sley"), &actual_root, name),
                    cat_tag(sley_testkit::oracle_git(), &expected_root, name),
                    "annotated tag object differed for {args:?}"
                );
            }
        }

        let errors_root = root.join("errors");
        fs::create_dir_all(&errors_root).expect("create errors repo");
        prepare_tag_message_repo(&errors_root);
        for args in [vec!["tag", "-u"], vec!["tag", "--local-user"]] {
            let expected = run_output(sley_testkit::oracle_git(), &errors_root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &errors_root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_list_ignore_case_sorts_metadata_like_upstream_git() {
    let root = unique_temp_dir("tag-list-ignore-case-sort-metadata");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_named_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "--allow-empty", "-m", "apple commit", "-q"],
            1000,
            "alice User",
            "alice@example.invalid",
        );
        let apple_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("apple HEAD oid is utf8")
            .trim()
            .to_string();
        run_with_named_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "--allow-empty", "-m", "Banana commit", "-q"],
            2000,
            "Bob User",
            "bob@example.invalid",
        );
        let banana_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("banana HEAD oid is utf8")
            .trim()
            .to_string();
        run_with_named_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "tag",
                "-a",
                "case-apple",
                "-m",
                "apple tag",
                apple_oid.as_str(),
            ],
            3000,
            "alice Tagger",
            "alice-tagger@example.invalid",
        );
        run_with_named_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "tag",
                "-a",
                "case-Banana",
                "-m",
                "Banana tag",
                banana_oid.as_str(),
            ],
            4000,
            "Bob Tagger",
            "bob-tagger@example.invalid",
        );

        let format = "%(refname:short)|%(contents:subject)|%(taggername)|%(*authorname)|%(*contents:subject)";
        for args in [
            vec![
                "tag",
                "-l",
                "case-*",
                "--sort=contents:subject",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--ignore-case",
                "--sort=contents:subject",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--sort=taggername",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--ignore-case",
                "--sort=taggername",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--sort=*authorname",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--ignore-case",
                "--sort=*authorname",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--sort=*contents:subject",
                "--format",
                format,
            ],
            vec![
                "tag",
                "-l",
                "case-*",
                "--ignore-case",
                "--sort=*contents:subject",
                "--format",
                format,
            ],
            vec!["tag", "-l", "case-*", "--sort=tag", "--format", format],
            vec![
                "tag",
                "-l",
                "case-*",
                "--ignore-case",
                "--sort=tag",
                "--format",
                format,
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_list_column_modes_match_upstream_git() {
    let root = unique_temp_dir("tag-list-column");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "--allow-empty", "-m", "initial", "-q"],
            1000,
        );
        for tag in ["alpha", "beta"] {
            git(&root, &["tag", tag]);
        }

        for args in [
            vec!["tag", "--column"],
            vec!["tag", "--column="],
            vec!["tag", "--column=always"],
            vec!["tag", "--column=column"],
            vec!["tag", "--column=row"],
            vec!["tag", "--column=dense"],
            vec!["tag", "--column=nodense"],
            vec!["tag", "--column=always,dense"],
            vec!["tag", "--column=dense,always"],
            vec!["tag", "--column=plain,column"],
            vec!["tag", "--column=always,plain"],
            vec!["tag", "--column=never,always"],
            vec!["tag", "--column=always,never"],
            vec!["tag", "--column=bad"],
            vec!["tag", "--column=dense,bad"],
            vec!["tag", "--no-column=foo"],
            vec!["tag", "--color=bad"],
            vec!["tag", "--no-color=foo"],
            vec!["tag", "--format"],
            vec!["tag", "--no-format=foo"],
            vec!["tag", "--sort"],
            vec!["tag", "--no-sort=foo"],
            vec!["tag", "--points-at"],
            vec!["tag", "--no-points-at=foo"],
            vec!["tag", "--omit-empty=foo"],
            vec!["tag", "--no-omit-empty=foo"],
            vec!["tag", "--ignore-case=foo"],
            vec!["tag", "--no-ignore-case=foo"],
            vec!["tag", "--list=foo"],
            vec!["tag", "--delete=foo"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tag_list_patterns_match_upstream_git() {
    let root = unique_temp_dir("tag-list-patterns");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-m",
                "initial body",
                "-q",
            ],
            1000,
        );
        let first_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("HEAD oid is utf8")
            .trim()
            .to_string();
        for tag in ["QA-2", "qa-1", "release/2026.05", "v1.0", "v10.0", "v2.0"] {
            git(&root, &["tag", tag]);
        }
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "annotated",
                "-m",
                "annotated",
            ],
            2000,
        );
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "annotated-lines",
                "-m",
                "tag subject",
                "-m",
                "tag body",
            ],
            3000,
        );
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "second",
                "-q",
            ],
            4000,
        );
        let second_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("second HEAD oid is utf8")
            .trim()
            .to_string();
        run_with_identity_at(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "annotated-second",
                "-m",
                "annotated second",
            ],
            5000,
        );
        git(&root, &["tag", "other"]);

        for args in [
            vec!["tag", "--color"],
            vec!["tag", "--color=always"],
            vec!["tag", "--color=auto"],
            vec!["tag", "--no-color"],
            vec!["tag", "--color=never"],
            vec!["tag", "--no-column"],
            vec!["tag", "--column=auto"],
            vec!["tag", "--column=never"],
            vec!["tag", "--column=plain"],
            vec!["tag", "--omit-empty"],
            vec!["tag", "--no-omit-empty"],
            vec!["tag", "-l", "v*"],
            vec!["tag", "-l", "--color", "v*"],
            vec!["tag", "-l", "--color=always", "v*"],
            vec!["tag", "-l", "--color=auto", "v*"],
            vec!["tag", "-l", "--no-color", "v*"],
            vec!["tag", "-l", "--color=never", "v*"],
            vec!["tag", "-l", "--no-column", "v*"],
            vec!["tag", "-l", "--column=auto", "v*"],
            vec!["tag", "-l", "--column=never", "v*"],
            vec!["tag", "-l", "--column=plain", "v*"],
            vec!["tag", "-l", "--omit-empty", "v*"],
            vec!["tag", "-l", "--omit-empty", "--no-omit-empty", "v*"],
            vec!["tag", "-l", "--", "v*"],
            vec!["tag", "--list", "--", "v*"],
            vec!["tag", "--format=%(refname:short)"],
            vec![
                "tag",
                "--format=%(refname)|%(refname:lstrip=2)|%(refname:rstrip=1)|%(objectname:short)|%(objectname:short=12)",
            ],
            vec!["tag", "-l", "--format", "%(refname)", "v*"],
            vec!["tag", "-l", "--format=%(refname:short) %(objecttype)", "v*"],
            vec!["tag", "-l", "--format=tag:%(refname:short):%%", "v*"],
            vec![
                "tag",
                "-l",
                "--color=always",
                "--format=%(color:red)%(refname:short)%(color:reset)",
                "v1.0",
            ],
            vec![
                "tag",
                "-l",
                "--format=%(refname:short) %(objecttype) %(*objecttype)",
                "annotated",
            ],
            vec![
                "tag",
                "-l",
                "--format=%(refname:short) %(taggername) %(contents:subject)",
                "annotated",
            ],
            vec![
                "tag",
                "-l",
                "--format=%(refname:short)|%(contents:lines=1)|%(taggerdate:short)|%(*authordate:unix)",
                "annotated*",
            ],
            vec![
                "tag",
                "-l",
                "--format=%(refname:short)",
                "--no-format",
                "v*",
            ],
            vec!["tag", "-l", "--format=", "--omit-empty", "v*"],
            vec!["tag", "-n", "annotated-lines"],
            vec!["tag", "-n2", "annotated-lines"],
            vec!["tag", "-n1k", "annotated-lines"],
            vec!["tag", "-n1K", "annotated-lines"],
            vec!["tag", "-n+1", "annotated-lines"],
            vec!["tag", "-n0k", "annotated-lines"],
            vec!["tag", "-n3", "v1.0"],
            vec!["tag", "-n0", "v1.0"],
            vec!["tag", "--list", "release/*", "qa-?"],
            vec!["tag", "-l", "q[ab]-?"],
            vec!["tag", "-l", "--ignore-case", "qa-?"],
            vec!["tag", "-l", "-i", "qa-?"],
            vec!["tag", "-l", "--ignore-case", "q[a-b]-?"],
            vec!["tag", "-l", "--ignore-case", "--no-ignore-case", "qa-?"],
            vec!["tag", "-l", "--sort=refname", "v*"],
            vec!["tag", "-l", "--sort=-refname", "v*"],
            vec!["tag", "-l", "--sort", "version:refname", "v*"],
            vec!["tag", "-l", "--sort=v:refname", "v*"],
            vec!["tag", "-l", "--sort=-version:refname", "v*"],
            vec!["tag", "-l", "--sort=objectname"],
            vec!["tag", "-l", "--sort=-objectname"],
            vec!["tag", "-l", "--sort", "objectname", "v*"],
            vec!["tag", "-l", "--sort", "-objectname", "v*"],
            vec!["tag", "-l", "--sort=objecttype"],
            vec!["tag", "-l", "--sort=-objecttype"],
            vec!["tag", "-l", "--sort", "objecttype", "a*"],
            vec!["tag", "-l", "--sort", "-objecttype", "a*"],
            vec!["tag", "-l", "--sort=objectsize"],
            vec!["tag", "-l", "--sort=-objectsize"],
            vec!["tag", "-l", "--sort", "objectsize", "a*"],
            vec!["tag", "-l", "--sort", "-objectsize", "a*"],
            vec!["tag", "-l", "--sort=objectsize:disk"],
            vec!["tag", "-l", "--sort=-objectsize:disk"],
            vec!["tag", "-l", "--sort", "objectsize:disk", "a*"],
            vec!["tag", "-l", "--sort", "-objectsize:disk", "a*"],
            vec!["tag", "-l", "--sort=deltabase"],
            vec!["tag", "-l", "--sort=-deltabase"],
            vec!["tag", "-l", "--sort", "raw:size"],
            vec!["tag", "-l", "--sort", "-raw:size"],
            vec!["tag", "-l", "--sort=*objectname"],
            vec!["tag", "-l", "--sort=-*objectname"],
            vec!["tag", "-l", "--sort", "*objecttype"],
            vec!["tag", "-l", "--sort", "-*objecttype"],
            vec!["tag", "-l", "--sort=*objectsize"],
            vec!["tag", "-l", "--sort=-*objectsize"],
            vec!["tag", "-l", "--sort", "*objectsize:disk", "a*"],
            vec!["tag", "-l", "--sort", "-*objectsize:disk", "a*"],
            vec!["tag", "-l", "--sort=*deltabase"],
            vec!["tag", "-l", "--sort=-*deltabase"],
            vec!["tag", "-l", "--sort", "*raw:size", "a*"],
            vec!["tag", "-l", "--sort", "-*raw:size", "a*"],
            vec!["tag", "-l", "--sort=authordate"],
            vec!["tag", "-l", "--sort=-authordate"],
            vec!["tag", "-l", "--sort", "committerdate"],
            vec!["tag", "-l", "--sort", "-committerdate"],
            vec!["tag", "-l", "--sort=taggerdate"],
            vec!["tag", "-l", "--sort=-taggerdate"],
            vec!["tag", "-l", "--sort", "creatordate"],
            vec!["tag", "-l", "--sort", "-creatordate"],
            vec!["tag", "-l", "--sort=taggerdate", "a*"],
            vec!["tag", "-l", "--sort=-creatordate", "a*"],
            vec!["tag", "-l", "--sort=*authordate"],
            vec!["tag", "-l", "--sort=-*authordate"],
            vec!["tag", "-l", "--sort", "*committerdate"],
            vec!["tag", "-l", "--sort", "-*committerdate"],
            vec!["tag", "-l", "--sort=*taggerdate"],
            vec!["tag", "-l", "--sort=-*taggerdate"],
            vec!["tag", "-l", "--sort", "*creatordate"],
            vec!["tag", "-l", "--sort", "-*creatordate"],
            vec!["tag", "-l", "--sort=*authordate", "a*"],
            vec!["tag", "-l", "--sort=-*creatordate", "a*"],
            vec!["tag", "-l", "--sort=author"],
            vec!["tag", "-l", "--sort=-author"],
            vec!["tag", "-l", "--sort", "authorname"],
            vec!["tag", "-l", "--sort", "-authoremail"],
            vec!["tag", "-l", "--sort=committer"],
            vec!["tag", "-l", "--sort=-committer"],
            vec!["tag", "-l", "--sort", "committername"],
            vec!["tag", "-l", "--sort", "-committeremail"],
            vec!["tag", "-l", "--sort=tagger"],
            vec!["tag", "-l", "--sort=-tagger"],
            vec!["tag", "-l", "--sort", "taggername"],
            vec!["tag", "-l", "--sort", "-taggeremail"],
            vec!["tag", "-l", "--sort=creator"],
            vec!["tag", "-l", "--sort=-creator"],
            vec!["tag", "-l", "--sort=taggername", "a*"],
            vec!["tag", "-l", "--sort=-authoremail", "v*"],
            vec!["tag", "-l", "--sort=*author"],
            vec!["tag", "-l", "--sort=-*author"],
            vec!["tag", "-l", "--sort", "*authorname"],
            vec!["tag", "-l", "--sort", "-*authoremail"],
            vec!["tag", "-l", "--sort=*committer"],
            vec!["tag", "-l", "--sort=-*committer"],
            vec!["tag", "-l", "--sort", "*committername"],
            vec!["tag", "-l", "--sort", "-*committeremail"],
            vec!["tag", "-l", "--sort=*tagger"],
            vec!["tag", "-l", "--sort=-*tagger"],
            vec!["tag", "-l", "--sort", "*taggername"],
            vec!["tag", "-l", "--sort", "-*taggeremail"],
            vec!["tag", "-l", "--sort=*creator"],
            vec!["tag", "-l", "--sort=-*creator"],
            vec!["tag", "-l", "--sort=*authorname", "a*"],
            vec!["tag", "-l", "--sort=-*committeremail", "v*"],
            vec!["tag", "-l", "--sort=tag"],
            vec!["tag", "-l", "--sort=-tag"],
            vec!["tag", "-l", "--sort", "type"],
            vec!["tag", "-l", "--sort", "-type"],
            vec!["tag", "-l", "--sort", "object"],
            vec!["tag", "-l", "--sort", "-object"],
            vec!["tag", "-l", "--sort=tag", "a*"],
            vec!["tag", "-l", "--sort=-object", "a*"],
            vec!["tag", "-l", "--sort=tree"],
            vec!["tag", "-l", "--sort=-tree"],
            vec!["tag", "-l", "--sort", "parent"],
            vec!["tag", "-l", "--sort", "-parent"],
            vec!["tag", "-l", "--sort=numparent"],
            vec!["tag", "-l", "--sort=-numparent"],
            vec!["tag", "-l", "--sort=*tree"],
            vec!["tag", "-l", "--sort=-*tree"],
            vec!["tag", "-l", "--sort", "*parent"],
            vec!["tag", "-l", "--sort", "-*parent"],
            vec!["tag", "-l", "--sort=*numparent"],
            vec!["tag", "-l", "--sort=-*numparent"],
            vec!["tag", "-l", "--sort=tree", "a*"],
            vec!["tag", "-l", "--sort=-*parent", "a*"],
            vec!["tag", "-l", "--sort=subject"],
            vec!["tag", "-l", "--sort=-subject"],
            vec!["tag", "-l", "--sort", "contents:subject"],
            vec!["tag", "-l", "--sort", "-contents:subject"],
            vec!["tag", "-l", "--sort=*subject"],
            vec!["tag", "-l", "--sort=-*subject"],
            vec!["tag", "-l", "--sort", "*contents:subject"],
            vec!["tag", "-l", "--sort", "-*contents:subject"],
            vec!["tag", "-l", "--sort=body"],
            vec!["tag", "-l", "--sort=-body"],
            vec!["tag", "-l", "--sort", "contents:body"],
            vec!["tag", "-l", "--sort", "-contents:body"],
            vec!["tag", "-l", "--sort=*body"],
            vec!["tag", "-l", "--sort=-*body"],
            vec!["tag", "-l", "--sort", "*contents:body"],
            vec!["tag", "-l", "--sort", "-*contents:body"],
            vec!["tag", "-l", "--sort=contents:size"],
            vec!["tag", "-l", "--sort=-contents:size"],
            vec!["tag", "-l", "--sort=*contents:size"],
            vec!["tag", "-l", "--sort=-*contents:size"],
            vec!["tag", "-l", "--sort=contents:subject", "a*"],
            vec!["tag", "-l", "--sort=-contents:size", "v*"],
            vec!["tag", "-l", "--sort=*contents:subject", "a*"],
            vec!["tag", "-l", "--sort=-*contents:size", "v*"],
            vec!["tag", "-l", "--sort=-refname", "--no-sort", "v*"],
            vec!["tag", "-l", "--sort=objectname", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=objectname"],
            vec!["tag", "-l", "--sort=objecttype", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-objecttype"],
            vec!["tag", "-l", "--sort=objectsize", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-objectsize"],
            vec!["tag", "-l", "--sort=objectsize:disk", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-objectsize:disk"],
            vec!["tag", "-l", "--sort=deltabase", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-raw:size"],
            vec!["tag", "-l", "--sort=*objectname", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*objecttype"],
            vec!["tag", "-l", "--sort=*deltabase", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*raw:size"],
            vec!["tag", "-l", "--sort=taggerdate", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-creatordate"],
            vec!["tag", "-l", "--sort=*authordate", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*creatordate"],
            vec!["tag", "-l", "--sort=taggername", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-committeremail"],
            vec!["tag", "-l", "--sort=*authorname", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*committeremail"],
            vec!["tag", "-l", "--sort=tag", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-type"],
            vec!["tag", "-l", "--sort=tree", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*numparent"],
            vec!["tag", "-l", "--sort=subject", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-contents:size"],
            vec!["tag", "-l", "--sort=*subject", "--no-sort"],
            vec!["tag", "-l", "--no-sort", "--sort=-*contents:size"],
            vec!["tag", "-l", "--sort=refname", "--sort=objectname"],
            vec!["tag", "-l", "--sort=objectname", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=objectsize"],
            vec!["tag", "-l", "--sort=objectsize", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=objectsize:disk"],
            vec!["tag", "-l", "--sort=objectsize:disk", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=raw:size"],
            vec!["tag", "-l", "--sort=deltabase", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=*objectname"],
            vec!["tag", "-l", "--sort=*objectsize", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=*raw:size"],
            vec!["tag", "-l", "--sort=refname", "--sort=creatordate"],
            vec!["tag", "-l", "--sort=creatordate", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=*authordate"],
            vec!["tag", "-l", "--sort=*creatordate", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=tagger"],
            vec!["tag", "-l", "--sort=tagger", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=*author"],
            vec!["tag", "-l", "--sort=*committer", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=tag"],
            vec!["tag", "-l", "--sort=tag", "--sort=refname"],
            vec!["tag", "-l", "--sort=object", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=tree"],
            vec!["tag", "-l", "--sort=*parent", "--sort=refname"],
            vec!["tag", "-l", "--sort=numparent", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=contents:subject"],
            vec!["tag", "-l", "--sort=contents:size", "--sort=refname"],
            vec!["tag", "-l", "--sort=refname", "--sort=*contents:subject"],
            vec!["tag", "-l", "--sort=*contents:size", "--sort=refname"],
            vec!["tag", "-l", "--ignore-case", "--sort=-refname", "qa-?"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let points_at_eq = format!("--points-at={first_oid}");
        for args in [
            vec!["tag", "--points-at", first_oid.as_str()],
            vec!["tag", "-l", "--points-at", first_oid.as_str(), "a*"],
            vec!["tag", points_at_eq.as_str()],
            vec!["tag", "--points-at", first_oid.as_str(), "--no-points-at"],
            vec!["tag", "--no-points-at", "--points-at", first_oid.as_str()],
            vec!["tag", "--points-at", first_oid.as_str(), "--", "a*"],
            vec!["tag", "--contains"],
            vec!["tag", "--contains", first_oid.as_str()],
            vec!["tag", "--contains", second_oid.as_str()],
            vec!["tag", "--no-contains"],
            vec!["tag", "--no-contains", second_oid.as_str()],
            vec!["tag", "--contains", first_oid.as_str(), "a*"],
            vec!["tag", "--merged"],
            vec!["tag", "--merged", first_oid.as_str()],
            vec!["tag", "--no-merged"],
            vec!["tag", "--no-merged", first_oid.as_str()],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        let contains_eq = format!("--contains={second_oid}");
        let expected = git(&root, &["tag", contains_eq.as_str()]);
        let actual = git_rs(&root, &["tag", contains_eq.as_str()]);
        assert_eq!(actual, expected, "sley output differed for --contains=");
        let no_contains_eq = format!("--no-contains={second_oid}");
        let expected = git(&root, &["tag", no_contains_eq.as_str()]);
        let actual = git_rs(&root, &["tag", no_contains_eq.as_str()]);
        assert_eq!(actual, expected, "sley output differed for --no-contains=");
        let merged_eq = format!("--merged={first_oid}");
        let expected = git(&root, &["tag", merged_eq.as_str()]);
        let actual = git_rs(&root, &["tag", merged_eq.as_str()]);
        assert_eq!(actual, expected, "sley output differed for --merged=");
        let no_merged_eq = format!("--no-merged={first_oid}");
        let expected = git(&root, &["tag", no_merged_eq.as_str()]);
        let actual = git_rs(&root, &["tag", no_merged_eq.as_str()]);
        assert_eq!(actual, expected, "sley output differed for --no-merged=");
    };
    let _ = fs::remove_dir_all(&root);
}
