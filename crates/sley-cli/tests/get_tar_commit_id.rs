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

fn run_output_with_input(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
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
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn assert_stdin_match(cwd: &Path, args: &[&str], stdin: &[u8]) {
    let expected = run_output_with_input(sley_testkit::oracle_git(), cwd, args, stdin);
    let actual = run_output_with_input(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    // `die_errno` embeds whatever errno happens to be at exit ("Success",
    // "No such file or directory", ...) — incidental process state that
    // legitimately differs across environments for BOTH binaries. Compare the
    // message with the strerror suffix stripped (the deterministic prefix is
    // still byte-compared); keep full equality when no errno suffix is present.
    let strip_errno = |stderr: &[u8]| -> Vec<u8> {
        let text = String::from_utf8_lossy(stderr);
        match text.find("EOF before reading tar header: ") {
            Some(pos) => text[..pos + "EOF before reading tar header: ".len()]
                .as_bytes()
                .to_vec(),
            None => stderr.to_vec(),
        }
    };
    assert_eq!(
        strip_errno(&actual.stderr),
        strip_errno(&expected.stderr),
        "sley stderr differed for {args:?}"
    );
}

#[test]
fn get_tar_commit_id_matches_upstream_git() {
    let root = unique_temp_dir("get-tar-commit-id");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        let status = Command::new(sley_testkit::oracle_git())
            .arg("init")
            .arg(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        std::fs::write(root.join("file.txt"), b"hello\n").expect("write file");
        let add = run_output(sley_testkit::oracle_git(), &root, &["add", "file.txt"]);
        assert!(add.status.success(), "git add failed");
        let commit = Command::new(sley_testkit::oracle_git())
            .current_dir(&root)
            .args(["commit", "-m", "initial"])
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
            .env("GIT_AUTHOR_DATE", "@1234567890 +0000")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
            .env("GIT_COMMITTER_DATE", "@1234567890 +0000")
            .output()
            .expect("run git commit");
        assert!(commit.status.success(), "git commit failed");

        let commit_tar = run_output(sley_testkit::oracle_git(), &root, &["archive", "--format=tar", "HEAD"]);
        assert!(commit_tar.status.success(), "git archive HEAD failed");
        let tree_tar = run_output(sley_testkit::oracle_git(), &root, &["archive", "--format=tar", "HEAD^{tree}"]);
        assert!(tree_tar.status.success(), "git archive tree failed");

        assert_stdin_match(&root, &["get-tar-commit-id"], &commit_tar.stdout);
        assert_stdin_match(&root, &["get-tar-commit-id"], &tree_tar.stdout);
        assert_stdin_match(&root, &["get-tar-commit-id"], b"");
        assert_stdin_match(&root, &["get-tar-commit-id"], b"not tar");
        assert_stdin_match(&root, &["get-tar-commit-id", "extra"], &commit_tar.stdout);
    }
    let _ = std::fs::remove_dir_all(&root);
}
