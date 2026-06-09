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

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(cwd: &Path, args: &[&str]) {
    run_success("git", cwd, args);
}

fn git_with_identity(cwd: &Path, args: &[&str]) {
    let output = run_output_with_identity("git", cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
    );
}

fn prepare_identity(root: &Path) {
    git(
        root,
        &[
            "config",
            "user.name",
            "Example User",
        ],
    );
    git(
        root,
        &[
            "config",
            "user.email",
            "example@example.invalid",
        ],
    );
}

fn prepare_diverged_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("shared.txt"), b"base\n").expect("write shared file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    let base = String::from_utf8(run_output("git", upstream, &["rev-parse", "HEAD"]).stdout)
        .expect("base oid utf8")
        .trim()
        .to_string();
    git(upstream, &["checkout", "-b", "topic", &base, "-q"]);
    fs::write(upstream.join("topic.txt"), b"topic-only\n").expect("write topic file");
    git(upstream, &["add", "topic.txt"]);
    git_with_identity(upstream, &["commit", "-m", "topic", "-q"]);
    git(upstream, &["checkout", "master", "-q"]);
    fs::write(upstream.join("main.txt"), b"main-only\n").expect("write main file");
    git(upstream, &["add", "main.txt"]);
    git_with_identity(upstream, &["commit", "-m", "main", "-q"]);
    git(upstream, &["checkout", "topic", "-q"]);
}

fn prepare_pull_rebase_clone(upstream: &Path, clone: &Path, rebase_config: Option<&str>) {
    prepare_diverged_upstream(clone);
    let upstream_arg = upstream.to_str().expect("upstream path is utf8");
    git(clone, &["remote", "add", "origin", upstream_arg]);
    git(clone, &["fetch", "origin", "-q"]);
    if let Some(value) = rebase_config {
        git(clone, &["config", "pull.rebase", value]);
    }
}

#[test]
fn pull_rebase_clean_matches_upstream_git() {
    let root = unique_temp_dir("pull-rebase-clean");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    let result = (|| {
        prepare_diverged_upstream(&upstream);
        prepare_pull_rebase_clone(&upstream, &expected, Some("true"));
        prepare_pull_rebase_clone(&upstream, &actual, Some("true"));
        let args = ["pull", "origin", "master"];
        let expected_output = run_output_with_identity("git", &expected, &args);
        let actual_output =
            run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_eq!(
            actual_output.status.code(),
            expected_output.status.code(),
            "status differed for pull rebase"
        );
        assert!(
            actual_output.status.success(),
            "sley pull --rebase failed: {}",
            String::from_utf8_lossy(&actual_output.stderr)
        );
        assert_eq!(
            run_output("git", &expected, &["rev-parse", "HEAD"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &actual, &["rev-parse", "HEAD"]).stdout,
            "HEAD differed after pull rebase"
        );
        assert_eq!(
            run_output("git", &expected, &["log", "--oneline"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &actual, &["log", "--oneline"]).stdout,
            "log order differed after pull rebase"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn pull_rebase_flag_matches_upstream_git() {
    let root = unique_temp_dir("pull-rebase-flag");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    let result = (|| {
        prepare_diverged_upstream(&upstream);
        prepare_pull_rebase_clone(&upstream, &expected, Some("false"));
        prepare_pull_rebase_clone(&upstream, &actual, Some("false"));
        let args = ["pull", "--rebase", "origin", "master"];
        let expected_output = run_output_with_identity("git", &expected, &args);
        let actual_output =
            run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_eq!(
            actual_output.status.code(),
            expected_output.status.code(),
            "status differed for pull --rebase"
        );
        assert!(
            actual_output.status.success(),
            "sley pull --rebase failed: {}",
            String::from_utf8_lossy(&actual_output.stderr)
        );
        assert_eq!(
            run_output("git", &expected, &["rev-parse", "HEAD"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &actual, &["rev-parse", "HEAD"]).stdout,
            "HEAD differed after pull --rebase"
        );
        assert_eq!(
            run_output("git", &expected, &["log", "--oneline"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &actual, &["log", "--oneline"]).stdout,
            "log order differed after pull --rebase"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}