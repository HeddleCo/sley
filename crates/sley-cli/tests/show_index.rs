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
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

#[test]
fn show_index_matches_upstream_git() {
    let root = unique_temp_dir("show-index");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        let status = Command::new(sley_testkit::oracle_git())
            .arg("init")
            .arg(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        std::fs::write(root.join("one.txt"), b"one\n").expect("write file");
        let add = run_output(sley_testkit::oracle_git(), &root, &["add", "one.txt"]);
        assert!(add.status.success(), "git add failed");
        let commit = Command::new(sley_testkit::oracle_git())
            .current_dir(&root)
            .args(["commit", "-m", "one"])
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.invalid")
            .env("GIT_AUTHOR_DATE", "@1 +0000")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.invalid")
            .env("GIT_COMMITTER_DATE", "@1 +0000")
            .output()
            .expect("run git commit");
        assert!(commit.status.success(), "git commit failed");
        let gc = run_output(sley_testkit::oracle_git(), &root, &["gc"]);
        assert!(gc.status.success(), "git gc failed");

        let pack_dir = root.join(".git/objects/pack");
        let idx = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
            .expect("idx file");
        let idx_bytes = std::fs::read(idx).expect("read idx");

        assert_stdin_match(&root, &["show-index"], &idx_bytes);
        assert_stdin_match(&root, &["show-index", "--object-format=sha1"], &idx_bytes);
        assert_stdin_match(
            &root,
            &["show-index", "--object-format", "sha1", "ignored"],
            &idx_bytes,
        );
        assert_stdin_match(&root, &["show-index", "--object-format"], &idx_bytes);
        assert_stdin_match(&root, &["show-index", "--object-format=bad"], &idx_bytes);
        assert_stdin_match(&root, &["show-index", "--unknown"], &idx_bytes);
        assert_stdin_match(&root, &["show-index"], b"");
        assert_stdin_match(&root, &["show-index"], b"not idx");
    }
    let _ = std::fs::remove_dir_all(&root);
}
