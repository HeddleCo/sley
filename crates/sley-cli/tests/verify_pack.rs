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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), cwd, args);
    let actual = run_output(env!("CARGO_BIN_EXE_sley"), cwd, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
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

fn create_packed_repo(root: &Path) -> PathBuf {
    fs::create_dir_all(root).expect("create repo root");
    let init = run_output(sley_testkit::oracle_git(), root, &["init", "-b", "main"]);
    assert!(init.status.success(), "git init failed");
    fs::write(root.join("file.txt"), b"payload\n").expect("write file");
    let add = run_output(sley_testkit::oracle_git(), root, &["add", "file.txt"]);
    assert!(add.status.success(), "git add failed");
    let commit = Command::new(sley_testkit::oracle_git())
        .current_dir(root)
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
    let gc = run_output(sley_testkit::oracle_git(), root, &["gc"]);
    assert!(gc.status.success(), "git gc failed");

    fs::read_dir(root.join(".git").join("objects").join("pack"))
        .expect("read pack dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
        .expect("idx file")
}

#[test]
fn verify_pack_matches_upstream_git_for_installed_pack_indexes() {
    let root = unique_temp_dir("verify-pack");
    let idx = create_packed_repo(&root);
    let idx_arg = idx.to_string_lossy().to_string();

    for args in [
        vec!["verify-pack"],
        vec!["verify-pack", "--bad", &idx_arg],
        vec!["verify-pack", "-x", &idx_arg],
        vec!["verify-pack", "--object-format"],
        vec!["verify-pack", "--object-format=bad", &idx_arg],
        vec!["verify-pack", &idx_arg],
        vec!["verify-pack", "-v", &idx_arg],
        vec!["verify-pack", "--verbose", &idx_arg],
        vec!["verify-pack", "-s", &idx_arg],
        vec!["verify-pack", "--stat-only", &idx_arg],
        vec!["verify-pack", "--object-format=sha1", &idx_arg],
        vec!["verify-pack", "--", &idx_arg],
    ] {
        assert_status_stdout_stderr_match(&root, &args);
    }

    let _ = fs::remove_dir_all(&root);
}
