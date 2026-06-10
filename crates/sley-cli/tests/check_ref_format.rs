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
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

#[test]
fn check_ref_format_matches_upstream_git() {
    let root = unique_temp_dir("check-ref-format");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        for args in [
            vec!["check-ref-format", "refs/heads/main"],
            vec!["check-ref-format", "main"],
            vec!["check-ref-format", "--allow-onelevel", "main"],
            vec!["check-ref-format", "refs/heads/.bad"],
            vec!["check-ref-format", "refs/heads/foo.lock/bar"],
            vec!["check-ref-format", "refs/heads/bad..name"],
            vec!["check-ref-format", "refs/heads/bad name"],
            vec!["check-ref-format", "refs/heads/@{bad"],
            vec!["check-ref-format", "--normalize", "/refs//heads/main"],
            vec!["check-ref-format", "--normalize", "main"],
            vec![
                "check-ref-format",
                "--normalize",
                "--allow-onelevel",
                "/main",
            ],
            vec!["check-ref-format", "--refspec-pattern", "refs/heads/*"],
            vec!["check-ref-format", "--refspec-pattern", "refs/heads/*/*"],
            vec!["check-ref-format", "--branch", "main"],
            vec!["check-ref-format", "--branch", "-bad"],
            vec!["check-ref-format"],
            vec!["check-ref-format", "refs/heads/main", "extra"],
            vec!["check-ref-format", "--unknown", "refs/heads/main"],
        ] {
            assert_status_stdout_stderr_match(&root, &args);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}
