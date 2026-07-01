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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_input(program: &str, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        input,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("read source entry");
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().expect("entry type").is_dir() {
            copy_dir(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).expect("copy file");
        }
    }
}

fn assert_status_stdout_stderr_match(upstream: &Path, actual: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), upstream, args);
    let actual_output = run_output(sley_testkit::sley_bin!(), actual, args);
    assert_eq!(
        actual_output.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual_output.stdout),
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert_eq!(
        actual_output.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual_output.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

fn assert_stdin_match(upstream: &Path, actual: &Path, args: &[&str], input: &[u8]) {
    let expected = run_with_input(sley_testkit::oracle_git(), upstream, args, input);
    let actual_output = run_with_input(sley_testkit::sley_bin!(), actual, args, input);
    assert_eq!(
        actual_output.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual_output.stdout),
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert_eq!(
        actual_output.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual_output.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

fn create_replace_fixture(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("create repo root");
    let init = run_output(sley_testkit::oracle_git(), root, &["init", "-b", "main"]);
    assert!(init.status.success(), "git init failed");

    let first = commit_empty(root, "one");
    let second = commit_empty(root, "two");
    (first, second)
}

fn commit_empty(root: &Path, message: &str) -> String {
    let commit = Command::new(sley_testkit::oracle_git())
        .current_dir(root)
        .args(["commit", "--allow-empty", "-m", message])
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1 +0000")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
        .env("GIT_COMMITTER_DATE", "@1 +0000")
        .output()
        .expect("run git commit");
    assert!(
        commit.status.success(),
        "git commit failed\nstderr:\n{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let rev = run_output(sley_testkit::oracle_git(), root, &["rev-parse", "HEAD"]);
    assert!(rev.status.success(), "git rev-parse failed");
    String::from_utf8(rev.stdout)
        .expect("utf8 oid")
        .trim()
        .to_string()
}

fn assert_replace_ref_matches(upstream: &Path, actual: &Path, object: &str) {
    let rel = Path::new(".git").join("refs").join("replace").join(object);
    assert_eq!(
        fs::read(upstream.join(&rel)).expect("read upstream replace ref"),
        fs::read(actual.join(&rel)).expect("read sley replace ref"),
        "replace ref file differed"
    );
}

#[test]
fn replace_matches_upstream_git_for_core_ref_flows() {
    let root = unique_temp_dir("replace");
    let base = root.join("base");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    let (first, second) = create_replace_fixture(&base);
    copy_dir(&base, &upstream);
    copy_dir(&base, &actual);

    for args in [
        vec!["replace", "--bad"],
        vec!["replace", "-x"],
        vec!["replace", "--format=bad"],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
    }

    assert_status_stdout_stderr_match(&upstream, &actual, &["replace", &first, &second]);
    assert_replace_ref_matches(&upstream, &actual, &first);

    for args in [
        vec!["replace"],
        vec!["replace", "--list"],
        vec!["replace", "-l", "????????????????????????????????????????"],
        vec!["replace", "--format=medium"],
        vec!["replace", "--format=long"],
        vec!["replace", &first, &second],
        vec!["cat-file", "-p", &first],
        vec!["cat-file", "-s", &first],
        vec!["--no-replace-objects", "cat-file", "-p", &first],
        vec!["--no-replace-objects", "cat-file", "-s", &first],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
    }
    assert_stdin_match(
        &upstream,
        &actual,
        &["cat-file", "--batch-check"],
        format!("{first}\n").as_bytes(),
    );

    assert_status_stdout_stderr_match(&upstream, &actual, &["replace", "-d", &first]);
    assert!(
        !upstream
            .join(".git")
            .join("refs")
            .join("replace")
            .join(&first)
            .exists(),
        "upstream replace ref should be deleted"
    );
    assert!(
        !actual
            .join(".git")
            .join("refs")
            .join("replace")
            .join(&first)
            .exists(),
        "sley replace ref should be deleted"
    );

    let _ = fs::remove_dir_all(&root);
}
