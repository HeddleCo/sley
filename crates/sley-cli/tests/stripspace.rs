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
        child.stdin.as_mut().expect("stdin should be piped"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str], stdin: &[u8]) {
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
fn stripspace_matches_upstream_git() {
    let root = unique_temp_dir("stripspace");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        let input = b"  a  \n\n\n# c\n b\n\n";
        for args in [
            vec!["stripspace"],
            vec!["stripspace", "--strip-comments"],
            vec!["stripspace", "-s"],
            vec!["stripspace", "--comment-lines"],
            vec!["stripspace", "-c"],
            vec!["stripspace", "--strip-comments", "--comment-lines"],
            vec!["stripspace", "extra"],
            vec!["stripspace", "--unknown"],
        ] {
            assert_status_stdout_stderr_match(&root, &args, input);
        }
        assert_status_stdout_stderr_match(&root, &["stripspace", "--comment-lines"], b"a");
    }
    let _ = std::fs::remove_dir_all(&root);
}
