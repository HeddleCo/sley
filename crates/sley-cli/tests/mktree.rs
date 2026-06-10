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
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin is piped"),
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

fn run_success_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let output = run_with_stdin(program, cwd, args, stdin);
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

fn prepare_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo dir");
    run_success(sley_testkit::oracle_git(), root, &["init", "-q"]);
}

fn git_rs() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

fn hash_blob(root: &Path, body: &[u8]) -> String {
    String::from_utf8(run_success_with_stdin(
        sley_testkit::oracle_git(),
        root,
        &["hash-object", "-w", "--stdin"],
        body,
    ))
    .expect("hash-object output is utf8")
    .trim()
    .to_string()
}

#[test]
fn mktree_basic_and_z_modes_match_upstream_git() {
    let root = unique_temp_dir("mktree-basic");
    let expected = root.join("expected");
    let actual = root.join("actual");
    {
        prepare_repo(&expected);
        prepare_repo(&actual);
        let expected_a = hash_blob(&expected, b"a");
        let expected_z = hash_blob(&expected, b"z");
        let actual_a = hash_blob(&actual, b"a");
        let actual_z = hash_blob(&actual, b"z");
        assert_eq!(actual_a, expected_a);
        assert_eq!(actual_z, expected_z);

        let input = format!("100644 blob {expected_z}\tz\n100644 blob {expected_a}\ta\n");
        let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &["mktree"], input.as_bytes());
        let actual_output = run_with_stdin(git_rs(), &actual, &["mktree"], input.as_bytes());
        let actual_tree = String::from_utf8(actual_output.stdout.clone())
            .expect("mktree output is utf8")
            .trim()
            .to_string();
        assert_same_output(actual_output, expected_output, &["mktree"]);
        let expected_tree = run_success(sley_testkit::oracle_git(), &expected, &["cat-file", "-p", &actual_tree]);
        let actual_tree = run_success(sley_testkit::oracle_git(), &actual, &["cat-file", "-p", &actual_tree]);
        assert_eq!(actual_tree, expected_tree);

        let mut nul_input = format!("100644 blob {expected_z}\tz").into_bytes();
        nul_input.push(0);
        nul_input.extend_from_slice(format!("100644 blob {expected_a}\ta").as_bytes());
        nul_input.push(0);
        let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &["mktree", "-z"], &nul_input);
        let actual_output = run_with_stdin(git_rs(), &actual, &["mktree", "-z"], &nul_input);
        assert_same_output(actual_output, expected_output, &["mktree", "-z"]);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mktree_missing_commit_and_batch_modes_match_upstream_git() {
    let root = unique_temp_dir("mktree-missing-batch");
    let expected = root.join("expected");
    let actual = root.join("actual");
    {
        prepare_repo(&expected);
        prepare_repo(&actual);
        let missing = "0000000000000000000000000000000000000000";
        let missing_blob = format!("100644 blob {missing}\tmissing\n");
        let expected_output =
            run_with_stdin(sley_testkit::oracle_git(), &expected, &["mktree"], missing_blob.as_bytes());
        let actual_output = run_with_stdin(git_rs(), &actual, &["mktree"], missing_blob.as_bytes());
        assert_same_output(actual_output, expected_output, &["mktree"]);

        let expected_output = run_with_stdin(
            sley_testkit::oracle_git(),
            &expected,
            &["mktree", "--missing"],
            missing_blob.as_bytes(),
        );
        let actual_output = run_with_stdin(
            git_rs(),
            &actual,
            &["mktree", "--missing"],
            missing_blob.as_bytes(),
        );
        assert_same_output(actual_output, expected_output, &["mktree", "--missing"]);

        let commit_entry = format!("160000 commit {missing}\tsubmodule\n");
        let expected_output =
            run_with_stdin(sley_testkit::oracle_git(), &expected, &["mktree"], commit_entry.as_bytes());
        let actual_output = run_with_stdin(git_rs(), &actual, &["mktree"], commit_entry.as_bytes());
        assert_same_output(actual_output, expected_output, &["mktree"]);

        let a = hash_blob(&expected, b"a");
        let b = hash_blob(&expected, b"b");
        hash_blob(&actual, b"a");
        hash_blob(&actual, b"b");
        let batch = format!("100644 blob {a}\ta\n\n100644 blob {b}\tb\n");
        let expected_output =
            run_with_stdin(sley_testkit::oracle_git(), &expected, &["mktree", "--batch"], batch.as_bytes());
        let actual_output =
            run_with_stdin(git_rs(), &actual, &["mktree", "--batch"], batch.as_bytes());
        assert_same_output(actual_output, expected_output, &["mktree", "--batch"]);
    };
    let _ = fs::remove_dir_all(&root);
}
