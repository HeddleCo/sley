use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
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

fn write_blob(root: &Path, name: &str, contents: &[u8]) -> String {
    fs::write(root.join(name), contents).expect("write blob input");
    let output = run_success("git", root, &["hash-object", "-w", name]);
    String::from_utf8(output)
        .expect("blob oid is utf-8")
        .trim()
        .to_string()
}

fn delete_ref(root: &Path, name: &str) {
    let _ = run("git", root, &["update-ref", "-d", name]);
}

fn set_ref(root: &Path, name: &str, oid: &str) {
    run_success("git", root, &["update-ref", name, oid]);
}

fn read_ref(root: &Path, name: &str) -> Vec<u8> {
    fs::read(root.join(".git").join(name)).expect("read ref")
}

fn reflog_exists(root: &Path, name: &str) -> bool {
    root.join(".git").join("logs").join(name).is_file()
}

fn ref_exists(root: &Path, name: &str) -> bool {
    root.join(".git").join(name).is_file()
}

fn empty_commit(root: &Path) -> String {
    let tree = run_success("git", root, &["mktree"]);
    let tree = String::from_utf8(tree)
        .expect("tree oid is utf-8")
        .trim()
        .to_string();
    let commit = run_success(
        "git",
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit-tree",
            tree.as_str(),
            "-m",
            "commit",
        ],
    );
    String::from_utf8(commit)
        .expect("commit oid is utf-8")
        .trim()
        .to_string()
}

#[test]
fn update_ref_old_oid_and_deref_options_match_upstream_git() {
    let root = unique_temp_dir("update-ref-old-oid");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);

        let oid = write_blob(&expected, "payload.txt", b"payload\n");
        let actual_oid = write_blob(&actual, "payload.txt", b"payload\n");
        assert_eq!(actual_oid, oid);
        let wrong_oid = write_blob(&expected, "wrong.txt", b"wrong\n");
        let actual_wrong_oid = write_blob(&actual, "wrong.txt", b"wrong\n");
        assert_eq!(actual_wrong_oid, wrong_oid);
        let missing_oid = "1111111111111111111111111111111111111111";
        let zero = "0000000000000000000000000000000000000000";

        delete_ref(&expected, "refs/tags/default-log");
        delete_ref(&actual, "refs/tags/default-log");
        let args = ["update-ref", "refs/tags/default-log", oid.as_str(), zero];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            reflog_exists(&actual, "refs/tags/default-log"),
            reflog_exists(&expected, "refs/tags/default-log")
        );

        delete_ref(&expected, "refs/tags/explicit-log");
        delete_ref(&actual, "refs/tags/explicit-log");
        let args = [
            "update-ref",
            "--create-reflog",
            "refs/tags/explicit-log",
            oid.as_str(),
            zero,
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            reflog_exists(&actual, "refs/tags/explicit-log"),
            reflog_exists(&expected, "refs/tags/explicit-log")
        );
        assert_eq!(
            read_ref(&actual, "refs/tags/explicit-log"),
            read_ref(&expected, "refs/tags/explicit-log")
        );

        let expected_commit = empty_commit(&expected);
        let actual_commit = empty_commit(&actual);
        delete_ref(&expected, "refs/heads/logged");
        delete_ref(&actual, "refs/heads/logged");
        let expected_args = ["update-ref", "refs/heads/logged", expected_commit.as_str()];
        let actual_args = ["update-ref", "refs/heads/logged", actual_commit.as_str()];
        let expected_output = run("git", &expected, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(
            reflog_exists(&actual, "refs/heads/logged"),
            reflog_exists(&expected, "refs/heads/logged")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/default",
                "refs/tags/alias-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/default",
                "refs/tags/alias-target",
            ],
        );
        let args = ["update-ref", "refs/alias/default", wrong_oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/alias/default"),
            read_ref(&expected, "refs/alias/default")
        );
        assert_eq!(
            read_ref(&actual, "refs/tags/alias-target"),
            read_ref(&expected, "refs/tags/alias-target")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/no-deref",
                "refs/tags/no-deref-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/no-deref",
                "refs/tags/no-deref-target",
            ],
        );
        let args = [
            "update-ref",
            "--no-deref",
            "refs/alias/no-deref",
            oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/alias/no-deref"),
            read_ref(&expected, "refs/alias/no-deref")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/delete-default",
                "refs/tags/delete-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/delete-default",
                "refs/tags/delete-target",
            ],
        );
        set_ref(&expected, "refs/tags/delete-target", &oid);
        set_ref(&actual, "refs/tags/delete-target", &oid);
        let args = ["update-ref", "-d", "refs/alias/delete-default"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/alias/delete-default"),
            read_ref(&expected, "refs/alias/delete-default")
        );
        assert_eq!(
            reflog_exists(&actual, "refs/tags/delete-target"),
            reflog_exists(&expected, "refs/tags/delete-target")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/delete-no-deref",
                "refs/tags/delete-no-deref-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/delete-no-deref",
                "refs/tags/delete-no-deref-target",
            ],
        );
        let args = [
            "update-ref",
            "--no-deref",
            "-d",
            "refs/alias/delete-no-deref",
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            reflog_exists(&actual, "refs/alias/delete-no-deref"),
            reflog_exists(&expected, "refs/alias/delete-no-deref")
        );

        let expected_head_commit = empty_commit(&expected);
        let actual_head_commit = empty_commit(&actual);
        let expected_args = ["update-ref", "HEAD", expected_head_commit.as_str()];
        let actual_args = ["update-ref", "HEAD", actual_head_commit.as_str()];
        let expected_output = run("git", &expected, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(read_ref(&actual, "HEAD"), read_ref(&expected, "HEAD"));
        assert_eq!(
            read_ref(&actual, "refs/heads/main"),
            format!("{actual_head_commit}\n").into_bytes()
        );

        delete_ref(&expected, "refs/tags/separated");
        delete_ref(&actual, "refs/tags/separated");
        let args = [
            "update-ref",
            "--",
            "refs/tags/separated",
            oid.as_str(),
            zero,
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/separated"),
            read_ref(&expected, "refs/tags/separated")
        );

        let args = [
            "update-ref",
            "-d",
            "--",
            "refs/tags/separated",
            oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        delete_ref(&expected, "refs/tags/delete-zero");
        delete_ref(&actual, "refs/tags/delete-zero");
        let args = ["update-ref", "refs/tags/delete-zero", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        let args = ["update-ref", "-d", "refs/tags/delete-zero", zero];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "-d", "refs/tags/missing-delete"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "-d", "refs/tags/missing-delete", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "refs/tags/delete-mismatch", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        let args = [
            "update-ref",
            "-d",
            "refs/tags/delete-mismatch",
            wrong_oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        delete_ref(&expected, "refs/tags/message");
        delete_ref(&actual, "refs/tags/message");
        let args = [
            "update-ref",
            "-mattached reason",
            "--",
            "refs/tags/message",
            oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/message"),
            read_ref(&expected, "refs/tags/message")
        );

        delete_ref(&expected, "refs/tags/new");
        delete_ref(&actual, "refs/tags/new");
        let args = [
            "update-ref",
            "--no-deref",
            "refs/tags/new",
            oid.as_str(),
            zero,
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/new"),
            read_ref(&expected, "refs/tags/new")
        );

        set_ref(&expected, "refs/tags/topic", &oid);
        set_ref(&actual, "refs/tags/topic", &oid);
        let args = [
            "update-ref",
            "--deref",
            "refs/tags/topic",
            oid.as_str(),
            oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/topic"),
            read_ref(&expected, "refs/tags/topic")
        );

        set_ref(&expected, "refs/tags/topic", &oid);
        set_ref(&actual, "refs/tags/topic", &oid);
        let args = ["update-ref", "refs/tags/topic", oid.as_str(), zero];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        delete_ref(&expected, "refs/tags/missing");
        delete_ref(&actual, "refs/tags/missing");
        let args = [
            "update-ref",
            "refs/tags/missing",
            oid.as_str(),
            oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        set_ref(&expected, "refs/tags/topic", &oid);
        set_ref(&actual, "refs/tags/topic", &oid);
        let args = [
            "update-ref",
            "refs/tags/topic",
            wrong_oid.as_str(),
            wrong_oid.as_str(),
        ];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "refs/heads/blob", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "HEAD", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "--no-deref", "HEAD", oid.as_str()];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "--no-deref", "-d", "HEAD"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(ref_exists(&actual, "HEAD"), ref_exists(&expected, "HEAD"));
        fs::write(
            expected.join(".git").join("HEAD"),
            b"ref: refs/heads/main\n",
        )
        .expect("restore expected HEAD");
        fs::write(actual.join(".git").join("HEAD"), b"ref: refs/heads/main\n")
            .expect("restore actual HEAD");

        let args = ["update-ref", "refs/tags/missing-object", missing_oid];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        set_ref(&expected, "refs/tags/zero-new", &oid);
        set_ref(&actual, "refs/tags/zero-new", &oid);
        let args = ["update-ref", "refs/tags/zero-new", zero];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual.join(".git").join("refs/tags/zero-new").exists(),
            expected.join(".git").join("refs/tags/zero-new").exists()
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_ref_reftable_repository_matches_upstream_git() {
    let root = unique_temp_dir("update-ref-reftable");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q", "--ref-format=reftable"]);
        run_success("git", &actual, &["init", "-q", "--ref-format=reftable"]);

        let oid = write_blob(&expected, "payload.txt", b"payload\n");
        let actual_oid = write_blob(&actual, "payload.txt", b"payload\n");
        assert_eq!(actual_oid, oid);

        for args in [
            vec!["update-ref", "refs/tags/rust", oid.as_str()],
            vec!["show-ref", "refs/tags/rust"],
            vec!["update-ref", "-d", "refs/tags/rust", oid.as_str()],
            vec!["show-ref", "refs/tags/rust"],
        ] {
            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_ref_stdin_basic_commands_match_upstream_git() {
    let root = unique_temp_dir("update-ref-stdin");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);

        let oid = write_blob(&expected, "payload.txt", b"payload\n");
        let actual_oid = write_blob(&actual, "payload.txt", b"payload\n");
        assert_eq!(actual_oid, oid);
        let wrong_oid = write_blob(&expected, "wrong.txt", b"wrong\n");
        let actual_wrong_oid = write_blob(&actual, "wrong.txt", b"wrong\n");
        assert_eq!(actual_wrong_oid, wrong_oid);
        let missing_oid = "1111111111111111111111111111111111111111";
        let zero = "0000000000000000000000000000000000000000";

        let args = ["update-ref", "--stdin"];
        let input = format!("update refs/tags/stdin {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin"),
            read_ref(&expected, "refs/tags/stdin")
        );

        let input = b"delete refs/tags/stdin\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = format!("create refs/tags/stdin-create {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-create"),
            read_ref(&expected, "refs/tags/stdin-create")
        );

        let input = format!("verify refs/tags/stdin-create {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);

        let input = format!("verify refs/tags/stdin-create {wrong_oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);

        for input in [
            "delete refs/tags/stdin-missing\n".to_string(),
            "verify refs/tags/stdin-missing\n".to_string(),
            "verify refs/tags/stdin-missing 0000000000000000000000000000000000000000\n".to_string(),
        ] {
            let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
            let actual_output =
                run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
            assert_same_output(actual_output, expected_output, &args);
        }

        let input = format!("update refs/heads/stdin-blob {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);

        let input = format!("update refs/tags/stdin-missing-object {missing_oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);

        set_ref(&expected, "refs/tags/stdin-zero-new", &oid);
        set_ref(&actual, "refs/tags/stdin-zero-new", &oid);
        let input = format!("update refs/tags/stdin-zero-new {zero}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-zero-new")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-zero-new")
                .exists()
        );

        let input = format!("create refs/tags/stdin-create-zero {zero}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);

        let input = format!(
            "update refs/tags/stdin-implicit-rollback {oid}\nverify refs/tags/stdin-implicit-missing {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-implicit-rollback")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-implicit-rollback")
                .exists()
        );

        let input = format!(
            "update refs/tags/stdin-implicit-duplicate {oid}\nupdate refs/tags/stdin-implicit-duplicate {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-implicit-duplicate")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-implicit-duplicate")
                .exists()
        );

        let input = format!("start\nupdate refs/tags/stdin-transaction {oid}\nprepare\ncommit\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-transaction"),
            read_ref(&expected, "refs/tags/stdin-transaction")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-no-deref",
                "refs/tags/stdin-no-deref-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-no-deref",
                "refs/tags/stdin-no-deref-target",
            ],
        );
        let input = format!("option no-deref\nupdate refs/alias/stdin-no-deref {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-no-deref"),
            read_ref(&expected, "refs/alias/stdin-no-deref")
        );

        let input = b"option create-reflog\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = b"start\nabort\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = format!("start\nupdate refs/tags/stdin-aborted {oid}\nabort\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual.join(".git").join("refs/tags/stdin-aborted").exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-aborted")
                .exists()
        );

        set_ref(&expected, "refs/tags/stdin-aborted-restore", &oid);
        set_ref(&actual, "refs/tags/stdin-aborted-restore", &oid);
        let input = format!("start\nupdate refs/tags/stdin-aborted-restore {wrong_oid}\nabort\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-aborted-restore"),
            read_ref(&expected, "refs/tags/stdin-aborted-restore")
        );

        let expected_head_commit = empty_commit(&expected);
        let actual_head_commit = empty_commit(&actual);
        let expected_input =
            format!("start\nupdate HEAD {expected_head_commit}\nabort\n").into_bytes();
        let actual_input = format!("start\nupdate HEAD {actual_head_commit}\nabort\n").into_bytes();
        let head_no_deref_args = ["update-ref", "--stdin", "--no-deref"];
        let expected_output =
            run_with_stdin("git", &expected, &head_no_deref_args, &expected_input);
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &head_no_deref_args,
            &actual_input,
        );
        assert_same_output(actual_output, expected_output, &head_no_deref_args);
        assert_eq!(read_ref(&actual, "HEAD"), read_ref(&expected, "HEAD"));

        let expected_input = format!(
            "update HEAD {expected_head_commit}\nverify refs/tags/stdin-head-missing {wrong_oid}\n"
        )
        .into_bytes();
        let actual_input = format!(
            "update HEAD {actual_head_commit}\nverify refs/tags/stdin-head-missing {wrong_oid}\n"
        )
        .into_bytes();
        let expected_output =
            run_with_stdin("git", &expected, &head_no_deref_args, &expected_input);
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &head_no_deref_args,
            &actual_input,
        );
        assert_same_output(actual_output, expected_output, &head_no_deref_args);
        assert_eq!(read_ref(&actual, "HEAD"), read_ref(&expected, "HEAD"));

        let input = format!("start\nupdate refs/tags/stdin-start-eof {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-start-eof")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-start-eof")
                .exists()
        );

        let input = format!("start\nupdate refs/tags/stdin-prepare-eof {oid}\nprepare\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-prepare-eof")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-prepare-eof")
                .exists()
        );

        let input = format!(
            "start\nupdate refs/tags/stdin-prepared-command {oid}\nprepare\nverify refs/tags/stdin-prepared-command {oid}\ncommit\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-prepared-command")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-prepared-command")
                .exists()
        );

        let input = b"start\nstart\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = format!("update refs/tags/stdin-prepare-eof {oid}\nprepare\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-prepare-eof")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-prepare-eof")
                .exists()
        );

        let input = format!("update refs/tags/stdin-prepare-commit {oid}\nprepare\ncommit\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-prepare-commit"),
            read_ref(&expected, "refs/tags/stdin-prepare-commit")
        );

        let input = format!("update refs/tags/stdin-before-start {oid}\nstart\n");
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-before-start")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-before-start")
                .exists()
        );

        let input = format!(
            "start\nupdate refs/tags/stdin-closed-kept {oid}\ncommit\nupdate refs/tags/stdin-closed-rejected {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-closed-kept"),
            read_ref(&expected, "refs/tags/stdin-closed-kept")
        );
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-closed-rejected")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-closed-rejected")
                .exists()
        );

        let input = format!(
            "start\nupdate refs/tags/stdin-abort-closed {oid}\nabort\nupdate refs/tags/stdin-abort-closed-rejected {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-abort-closed")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-abort-closed")
                .exists()
        );

        let input = b"start\ncommit\nstart\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = format!(
            "start\nupdate refs/tags/stdin-duplicate {oid}\nupdate refs/tags/stdin-duplicate {wrong_oid}\nprepare\ncommit\n"
        );
        let expected_output = run_with_stdin("git", &expected, &args, input.as_bytes());
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input.as_bytes());
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-duplicate")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-duplicate")
                .exists()
        );

        let z_args = ["update-ref", "--stdin", "-z"];
        let input = format!("update refs/tags/stdin-z\0{oid}\0\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-z"),
            read_ref(&expected, "refs/tags/stdin-z")
        );

        let input = format!("create refs/tags/stdin-z-create\0{oid}\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-z-create"),
            read_ref(&expected, "refs/tags/stdin-z-create")
        );

        let input = format!("verify refs/tags/stdin-z-create\0{oid}\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        let input = format!("delete refs/tags/stdin-z-create\0{oid}\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        set_ref(&expected, "refs/tags/stdin-z-zero-new", &oid);
        set_ref(&actual, "refs/tags/stdin-z-zero-new", &oid);
        let input = format!("update refs/tags/stdin-z-zero-new\0{zero}\0\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-zero-new")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-zero-new")
                .exists()
        );

        let input = format!("create refs/tags/stdin-z-create-zero\0{zero}\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        let input = b"verify refs/tags/stdin-z-create\0\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        let input = format!(
            "update refs/tags/stdin-z-implicit-duplicate\0{oid}\0\0update refs/tags/stdin-z-implicit-duplicate\0{wrong_oid}\0\0"
        )
        .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-implicit-duplicate")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-implicit-duplicate")
                .exists()
        );

        let input =
            format!("start\0update refs/tags/stdin-z-transaction\0{oid}\0\0prepare\0commit\0")
                .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-z-transaction"),
            read_ref(&expected, "refs/tags/stdin-z-transaction")
        );

        let input = b"start\0abort\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        let input =
            format!("start\0update refs/tags/stdin-z-aborted\0{oid}\0\0abort\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-aborted")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-aborted")
                .exists()
        );

        let input = format!(
            "start\0update refs/tags/stdin-z-duplicate\0{oid}\0\0update refs/tags/stdin-z-duplicate\0{wrong_oid}\0\0commit\0"
        )
        .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-duplicate")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-duplicate")
                .exists()
        );

        let input = format!(
            "start\0update refs/tags/stdin-z-prepared-command\0{oid}\0\0prepare\0update refs/tags/stdin-z-prepared-after\0{wrong_oid}\0\0commit\0"
        )
        .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-prepared-command")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-prepared-command")
                .exists()
        );

        let input = b"start\0start\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);

        let input = format!(
            "start\0update refs/tags/stdin-z-closed-kept\0{oid}\0\0commit\0update refs/tags/stdin-z-closed-rejected\0{wrong_oid}\0\0"
        )
        .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-z-closed-kept"),
            read_ref(&expected, "refs/tags/stdin-z-closed-kept")
        );
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-z-closed-rejected")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-z-closed-rejected")
                .exists()
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-z-no-deref",
                "refs/tags/stdin-z-no-deref-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-z-no-deref",
                "refs/tags/stdin-z-no-deref-target",
            ],
        );
        let input =
            format!("option no-deref\0update refs/alias/stdin-z-no-deref\0{oid}\0\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-z-no-deref"),
            read_ref(&expected, "refs/alias/stdin-z-no-deref")
        );

        let input = b"symref-create refs/alias/stdin-sym refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-sym"),
            read_ref(&expected, "refs/alias/stdin-sym")
        );

        let no_deref_args = ["update-ref", "--stdin", "--no-deref"];
        let input = b"symref-verify refs/alias/stdin-sym refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);

        let input = b"option no-deref\nsymref-verify refs/alias/stdin-sym refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = b"symref-create refs/alias/stdin-sym-implicit-duplicate refs/heads/main\nsymref-update refs/alias/stdin-sym-implicit-duplicate refs/heads/next\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/alias/stdin-sym-implicit-duplicate")
                .exists(),
            expected
                .join(".git")
                .join("refs/alias/stdin-sym-implicit-duplicate")
                .exists()
        );

        for input in [
            b"symref-verify refs/alias/stdin-sym refs/heads/other\n".as_slice(),
            b"symref-verify refs/alias/stdin-sym\n".as_slice(),
            b"symref-verify refs/alias/missing refs/heads/main\n".as_slice(),
        ] {
            let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
            let actual_output =
                run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
            assert_same_output(actual_output, expected_output, &no_deref_args);
        }

        let input = b"symref-verify refs/alias/stdin-sym refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = b"symref-verify refs/alias/missing\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);

        let input = b"symref-update refs/alias/stdin-sym-update refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-sym-update"),
            read_ref(&expected, "refs/alias/stdin-sym-update")
        );

        let input =
            b"symref-update refs/alias/stdin-sym-update refs/heads/next ref refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-sym-update"),
            read_ref(&expected, "refs/alias/stdin-sym-update")
        );

        let input =
            b"symref-update refs/alias/stdin-sym-update refs/heads/other ref refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);

        set_ref(&expected, "refs/tags/stdin-sym-oid-direct", &oid);
        set_ref(&actual, "refs/tags/stdin-sym-oid-direct", &oid);
        let input =
            format!("symref-update refs/tags/stdin-sym-oid-direct refs/heads/main oid {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &no_deref_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &no_deref_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-sym-oid-direct"),
            read_ref(&expected, "refs/tags/stdin-sym-oid-direct")
        );

        let input = format!(
            "symref-update refs/tags/stdin-sym-oid-direct refs/heads/next oid {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &no_deref_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &no_deref_args);

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-deref",
                "refs/heads/stdin-sym-deref-target",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-deref",
                "refs/heads/stdin-sym-deref-target",
            ],
        );
        let input = b"symref-update refs/alias/stdin-sym-deref refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            read_ref(&actual, "refs/heads/stdin-sym-deref-target"),
            read_ref(&expected, "refs/heads/stdin-sym-deref-target")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-delete",
                "refs/heads/main",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-delete",
                "refs/heads/main",
            ],
        );
        let input = b"symref-delete refs/alias/stdin-sym-delete refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);
        assert!(
            !actual
                .join(".git")
                .join("refs/alias/stdin-sym-delete")
                .exists(),
            "sley left deleted symbolic ref"
        );

        let input = b"symref-delete refs/alias/stdin-sym-missing\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-delete-mismatch",
                "refs/heads/main",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-sym-delete-mismatch",
                "refs/heads/main",
            ],
        );
        let input = b"symref-delete refs/alias/stdin-sym-delete-mismatch refs/heads/other\n";
        let expected_output = run_with_stdin("git", &expected, &no_deref_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_deref_args, input);
        assert_same_output(actual_output, expected_output, &no_deref_args);

        let input = b"symref-delete refs/alias/stdin-sym-delete-mismatch refs/heads/main\n";
        let expected_output = run_with_stdin("git", &expected, &args, input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, input);
        assert_same_output(actual_output, expected_output, &args);

        let input = b"symref-create refs/alias/stdin-z-sym\0refs/heads/main\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_args, &input);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &z_args, &input);
        assert_same_output(actual_output, expected_output, &z_args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-z-sym"),
            read_ref(&expected, "refs/alias/stdin-z-sym")
        );

        let z_no_deref_args = ["update-ref", "--stdin", "-z", "--no-deref"];
        let input = b"symref-verify refs/alias/stdin-z-sym\0refs/heads/main\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_no_deref_args, &input);
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &z_no_deref_args,
            &input,
        );
        assert_same_output(actual_output, expected_output, &z_no_deref_args);

        let input =
            b"symref-update refs/alias/stdin-z-sym\0refs/heads/next\0ref\0refs/heads/main\0"
                .to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_no_deref_args, &input);
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &z_no_deref_args,
            &input,
        );
        assert_same_output(actual_output, expected_output, &z_no_deref_args);
        assert_eq!(
            read_ref(&actual, "refs/alias/stdin-z-sym"),
            read_ref(&expected, "refs/alias/stdin-z-sym")
        );

        run_success(
            "git",
            &expected,
            &[
                "symbolic-ref",
                "refs/alias/stdin-z-sym-delete",
                "refs/heads/main",
            ],
        );
        run_success(
            "git",
            &actual,
            &[
                "symbolic-ref",
                "refs/alias/stdin-z-sym-delete",
                "refs/heads/main",
            ],
        );
        let input = b"symref-delete refs/alias/stdin-z-sym-delete\0refs/heads/main\0".to_vec();
        let expected_output = run_with_stdin("git", &expected, &z_no_deref_args, &input);
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &z_no_deref_args,
            &input,
        );
        assert_same_output(actual_output, expected_output, &z_no_deref_args);

        let batch_args = ["update-ref", "--stdin", "--batch-updates"];
        let input = format!("update refs/tags/stdin-batch {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch"),
            read_ref(&expected, "refs/tags/stdin-batch")
        );

        set_ref(&expected, "refs/tags/stdin-batch-zero-new", &oid);
        set_ref(&actual, "refs/tags/stdin-batch-zero-new", &oid);
        let input = format!("update refs/tags/stdin-batch-zero-new {zero}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);
        assert_eq!(
            actual
                .join(".git")
                .join("refs/tags/stdin-batch-zero-new")
                .exists(),
            expected
                .join(".git")
                .join("refs/tags/stdin-batch-zero-new")
                .exists()
        );

        let input = format!("create refs/tags/stdin-batch-create-zero {zero}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);

        let input = format!("update refs/heads/stdin-batch-blob {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);

        let input = format!("update refs/tags/stdin-batch-missing-object {missing_oid}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);

        let batch_alias_args = ["update-ref", "--stdin", "-0"];
        let input = format!("update refs/tags/stdin-batch-alias {oid}\n");
        let expected_output = run_with_stdin("git", &expected, &batch_alias_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_alias_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_alias_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-alias"),
            read_ref(&expected, "refs/tags/stdin-batch-alias")
        );

        let batch_z_args = ["update-ref", "--stdin", "-z", "--batch-updates"];
        let input = format!("update refs/tags/stdin-batch-z\0{oid}\0\0").into_bytes();
        let expected_output = run_with_stdin("git", &expected, &batch_z_args, &input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &batch_z_args, &input);
        assert_same_output(actual_output, expected_output, &batch_z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-z"),
            read_ref(&expected, "refs/tags/stdin-batch-z")
        );

        let input = format!(
            "update refs/tags/stdin-batch-ok-before {oid}\n\
             update refs/tags/stdin-batch-missing {wrong_oid} {oid}\n\
             update refs/tags/stdin-batch-ok-after {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-ok-before"),
            read_ref(&expected, "refs/tags/stdin-batch-ok-before")
        );
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-ok-after"),
            read_ref(&expected, "refs/tags/stdin-batch-ok-after")
        );

        for name in [
            "stdin-batch-existing",
            "stdin-batch-mismatch",
            "stdin-batch-delete-mismatch",
            "stdin-batch-verify-mismatch",
        ] {
            set_ref(&expected, &format!("refs/tags/{name}"), &oid);
            set_ref(&actual, &format!("refs/tags/{name}"), &oid);
        }
        let input = format!(
            "create refs/tags/stdin-batch-existing {wrong_oid}\n\
             update refs/tags/stdin-batch-mismatch {wrong_oid} {wrong_oid}\n\
             delete refs/tags/stdin-batch-delete-mismatch {wrong_oid}\n\
             verify refs/tags/stdin-batch-verify-mismatch {wrong_oid}\n\
             update refs/tags/stdin-batch-ok-tail {wrong_oid}\n"
        );
        let expected_output = run_with_stdin("git", &expected, &batch_args, input.as_bytes());
        let actual_output = run_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &batch_args,
            input.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &batch_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-ok-tail"),
            read_ref(&expected, "refs/tags/stdin-batch-ok-tail")
        );

        let input = format!(
            "update refs/tags/stdin-batch-z-missing\0{wrong_oid}\0{oid}\0\
             update refs/tags/stdin-batch-z-ok-after\0{oid}\0\0"
        )
        .into_bytes();
        let expected_output = run_with_stdin("git", &expected, &batch_z_args, &input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &batch_z_args, &input);
        assert_same_output(actual_output, expected_output, &batch_z_args);
        assert_eq!(
            read_ref(&actual, "refs/tags/stdin-batch-z-ok-after"),
            read_ref(&expected, "refs/tags/stdin-batch-z-ok-after")
        );

        let no_batch_args = [
            "update-ref",
            "--stdin",
            "--batch-updates",
            "--no-batch-updates",
        ];
        let input = b"start\nabort\n";
        let expected_output = run_with_stdin("git", &expected, &no_batch_args, input);
        let actual_output =
            run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &no_batch_args, input);
        assert_same_output(actual_output, expected_output, &no_batch_args);

        let args = ["update-ref", "--batch-updates"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["update-ref", "-z", "--batch-updates"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
