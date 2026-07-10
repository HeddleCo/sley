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

fn run(program: &str, cwd: &Path, args: &[String], stdin: &[u8]) -> Output {
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
    child.wait_with_output().expect("wait for command")
}

fn assert_same(actual: &Output, expected: &Output, args: &[String]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differs for {args:?}\nexpected stderr:\n{}\nactual stderr:\n{}",
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differs for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differs for {args:?}"
    );
}

#[test]
fn explicit_git_dir_session_drives_hash_and_cat_file_repository_state() {
    let root = unique_temp_dir("session-object-plumbing");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repository");
    fs::create_dir_all(&actual).expect("create actual repository");

    for repo in [&expected, &actual] {
        let init = Command::new(sley_testkit::oracle_git())
            .current_dir(repo)
            .args(["init", "-q", "--object-format=sha256"])
            .output()
            .expect("initialize sha256 repository");
        assert!(init.status.success(), "git init failed: {init:?}");
    }

    let payload = b"explicit session object\n";
    let expected_hash_args = vec![
        format!("--git-dir={}", expected.join(".git").display()),
        "hash-object".to_string(),
        "-w".to_string(),
        "--stdin".to_string(),
    ];
    let actual_hash_args = vec![
        format!("--git-dir={}", actual.join(".git").display()),
        "hash-object".to_string(),
        "-w".to_string(),
        "--stdin".to_string(),
    ];
    let expected_hash = run(
        sley_testkit::oracle_git(),
        &root,
        &expected_hash_args,
        payload,
    );
    let actual_hash = run(sley_testkit::sley_bin!(), &root, &actual_hash_args, payload);
    assert_same(&actual_hash, &expected_hash, &actual_hash_args);

    let oid = String::from_utf8(expected_hash.stdout)
        .expect("oid is utf8")
        .trim()
        .to_string();
    let expected_cat_args = vec![
        format!("--git-dir={}", expected.join(".git").display()),
        "cat-file".to_string(),
        "--batch".to_string(),
    ];
    let actual_cat_args = vec![
        format!("--git-dir={}", actual.join(".git").display()),
        "cat-file".to_string(),
        "--batch".to_string(),
    ];
    let mut query = oid.into_bytes();
    query.push(b'\n');
    let expected_cat = run(
        sley_testkit::oracle_git(),
        &root,
        &expected_cat_args,
        &query,
    );
    let actual_cat = run(sley_testkit::sley_bin!(), &root, &actual_cat_args, &query);
    assert_same(&actual_cat, &expected_cat, &actual_cat_args);

    fs::remove_dir_all(root).expect("remove fixture");
}
