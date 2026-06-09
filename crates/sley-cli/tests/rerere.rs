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

fn init_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    std::fs::create_dir_all(&root).expect("create temp dir");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&root)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");
    root
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str]) {
    let expected = run_output("git", cwd, args);
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
fn rerere_status_empty_matches_git() {
    let repo = init_repo("rerere-status-empty");
    assert_status_stdout_stderr_match(&repo, &["rerere", "status"]);
}

#[test]
fn rerere_clear_on_empty_matches_git() {
    let repo = init_repo("rerere-clear-empty");
    assert_status_stdout_stderr_match(&repo, &["rerere", "clear"]);
}

#[test]
fn rerere_no_args_matches_git() {
    let repo = init_repo("rerere-no-args");
    assert_status_stdout_stderr_match(&repo, &["rerere"]);
}

#[test]
fn rerere_forget_without_paths_matches_git() {
    let repo = init_repo("rerere-forget-no-paths");
    assert_status_stdout_stderr_match(&repo, &["rerere", "forget"]);
}

#[test]
fn rerere_unknown_option_matches_git() {
    let repo = init_repo("rerere-unknown-option");
    assert_status_stdout_stderr_match(&repo, &["rerere", "--unknown"]);
}

#[test]
fn rerere_unknown_subcommand_matches_git() {
    let repo = init_repo("rerere-unknown-subcommand");
    assert_status_stdout_stderr_match(&repo, &["rerere", "unknown"]);
}
