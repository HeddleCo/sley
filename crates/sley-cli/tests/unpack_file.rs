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

fn assert_status_stderr_match(cwd: &Path, args: &[&str]) {
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
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

fn unpack_file(program: &str, cwd: &Path, arg: &str) -> (String, Vec<u8>) {
    let output = run_output(program, cwd, &["unpack-file", arg]);
    assert!(
        output.status.success(),
        "{program} unpack-file failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{program} wrote unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = String::from_utf8(output.stdout).expect("utf8 stdout");
    let path = path.trim_end_matches('\n').to_string();
    assert!(
        path.starts_with(".merge_file_"),
        "{program} temp path used unexpected prefix: {path}"
    );
    let contents = fs::read(cwd.join(&path)).expect("read unpacked file");
    (path, contents)
}

#[test]
fn unpack_file_matches_upstream_git() {
    let root = unique_temp_dir("unpack-file");
    fs::create_dir_all(&root).expect("create temp dir");
    {
        let init = run_output(sley_testkit::oracle_git(), &root, &["init", "-b", "main"]);
        assert!(init.status.success(), "git init failed");
        fs::write(root.join("file.txt"), b"payload\n").expect("write file");
        let add = run_output(sley_testkit::oracle_git(), &root, &["add", "file.txt"]);
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

        for args in [
            vec!["unpack-file"],
            vec!["unpack-file", "HEAD", "extra"],
            vec!["unpack-file", "missing"],
            vec!["unpack-file", "--bad"],
            vec!["unpack-file", "HEAD"],
            vec!["unpack-file", "HEAD^{tree}"],
        ] {
            assert_status_stderr_match(&root, &args);
        }

        let blob = run_output(
            sley_testkit::oracle_git(),
            &root,
            &["rev-parse", "HEAD:file.txt"],
        );
        assert!(blob.status.success(), "git rev-parse failed");
        let blob = String::from_utf8(blob.stdout).expect("utf8 oid");
        let blob = blob.trim();
        let (_git_path, git_contents) = unpack_file(sley_testkit::oracle_git(), &root, blob);
        let (_sley_path, sley_contents) = unpack_file(env!("CARGO_BIN_EXE_sley"), &root, blob);
        assert_eq!(
            sley_contents, git_contents,
            "unpacked blob contents differed"
        );

        let (_git_path, git_contents) =
            unpack_file(sley_testkit::oracle_git(), &root, "HEAD:file.txt");
        let (_sley_path, sley_contents) =
            unpack_file(env!("CARGO_BIN_EXE_sley"), &root, "HEAD:file.txt");
        assert_eq!(
            sley_contents, git_contents,
            "revision path unpacked contents differed"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
