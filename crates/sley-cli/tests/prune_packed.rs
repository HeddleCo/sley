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

fn loose_object_path(repo: &Path, oid: &str) -> PathBuf {
    repo.join(".git")
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..])
}

fn create_repo_with_packable_loose_objects(root: &Path) -> String {
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
    let blob = run_output(
        sley_testkit::oracle_git(),
        root,
        &["rev-parse", "HEAD:file.txt"],
    );
    assert!(blob.status.success(), "git rev-parse failed");
    let blob = String::from_utf8(blob.stdout).expect("utf8 oid");
    let blob = blob.trim().to_string();

    let object_list = run_output(
        sley_testkit::oracle_git(),
        root,
        &["rev-list", "--objects", "--all"],
    );
    assert!(object_list.status.success(), "git rev-list failed");
    let pack = run_with_input(
        sley_testkit::oracle_git(),
        root,
        &["pack-objects", ".git/objects/pack/pack-test"],
        &object_list.stdout,
    );
    assert!(pack.status.success(), "git pack-objects failed");
    assert!(
        loose_object_path(root, &blob).exists(),
        "fixture should keep loose object after manual pack"
    );
    blob
}

#[test]
fn prune_packed_matches_upstream_git() {
    let root = unique_temp_dir("prune-packed");
    let base = root.join("base");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    let blob = create_repo_with_packable_loose_objects(&base);
    copy_dir(&base, &upstream);
    copy_dir(&base, &actual);

    {
        for args in [
            vec!["prune-packed", "-n"],
            vec!["prune-packed", "--dry-run"],
            vec!["prune-packed", "-q", "-n"],
            vec!["prune-packed", "--no-dry-run", "--dry-run"],
            vec!["prune-packed", "--quiet", "--no-quiet", "--dry-run"],
            vec!["prune-packed", "--unknown"],
            vec!["prune-packed", "-v"],
            vec!["prune-packed", "extra"],
        ] {
            assert_status_stdout_stderr_match(&upstream, &actual, &args);
        }
    }
    assert!(
        loose_object_path(&upstream, &blob).exists(),
        "upstream dry-run should keep loose object"
    );
    assert!(
        loose_object_path(&actual, &blob).exists(),
        "sley dry-run should keep loose object"
    );

    assert_status_stdout_stderr_match(&upstream, &actual, &["prune-packed"]);
    assert!(
        !loose_object_path(&upstream, &blob).exists(),
        "upstream should remove packed loose object"
    );
    assert!(
        !loose_object_path(&actual, &blob).exists(),
        "sley should remove packed loose object"
    );

    let _ = fs::remove_dir_all(&root);
}
