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
    let actual_output = run_output(env!("CARGO_BIN_EXE_sley"), actual, args);
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

fn loose_object_path(repo: &Path, oid: &str) -> PathBuf {
    repo.join(".git")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..])
}

fn create_prune_fixture(root: &Path) -> String {
    fs::create_dir_all(root).expect("create repo root");
    let init = run_output(sley_testkit::oracle_git(), root, &["init", "-b", "main"]);
    assert!(init.status.success(), "git init failed");
    fs::write(root.join("file.txt"), b"reachable\n").expect("write file");
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

    let dangling = run_with_input(
        sley_testkit::oracle_git(),
        root,
        &["hash-object", "-w", "--stdin"],
        b"dangling\n",
    );
    assert!(dangling.status.success(), "git hash-object failed");
    String::from_utf8(dangling.stdout)
        .expect("utf8 oid")
        .trim()
        .to_string()
}

#[test]
fn prune_matches_upstream_git() {
    let root = unique_temp_dir("prune");
    let base = root.join("base");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    let dangling = create_prune_fixture(&base);
    copy_dir(&base, &upstream);
    copy_dir(&base, &actual);

    for args in [
        vec!["prune", "--bad"],
        vec!["prune", "-x"],
        vec!["prune", "--expire"],
        vec!["prune", "--no-expire=false"],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
    }

    for args in [
        vec!["prune", "-n"],
        vec!["prune", "-nv"],
        vec!["prune", "--dry-run", "--verbose"],
        vec!["prune", "--expire=never", "-n"],
        vec!["prune", "--no-expire", "-n"],
    ] {
        assert_status_stdout_stderr_match(&upstream, &actual, &args);
    }
    assert!(
        loose_object_path(&upstream, &dangling).exists(),
        "upstream dry-run should keep dangling loose object"
    );
    assert!(
        loose_object_path(&actual, &dangling).exists(),
        "sley dry-run should keep dangling loose object"
    );

    assert_status_stdout_stderr_match(&upstream, &actual, &["prune", "-v"]);
    assert!(
        !loose_object_path(&upstream, &dangling).exists(),
        "upstream should remove dangling loose object"
    );
    assert!(
        !loose_object_path(&actual, &dangling).exists(),
        "sley should remove dangling loose object"
    );

    let dangling = create_prune_fixture(&base.join("second"));
    let upstream_second = root.join("upstream-second");
    let actual_second = root.join("actual-second");
    copy_dir(&base.join("second"), &upstream_second);
    copy_dir(&base.join("second"), &actual_second);
    assert_status_stdout_stderr_match(&upstream_second, &actual_second, &["prune"]);
    assert!(
        !loose_object_path(&upstream_second, &dangling).exists(),
        "upstream plain prune should remove dangling loose object"
    );
    assert!(
        !loose_object_path(&actual_second, &dangling).exists(),
        "sley plain prune should remove dangling loose object"
    );

    let _ = fs::remove_dir_all(&root);
}
