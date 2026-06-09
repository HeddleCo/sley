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

fn prepare_diverged_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("shared.txt"), b"base\n").expect("write shared file");
        git(root, &["add", "shared.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        let base = String::from_utf8(run_output("git", root, &["rev-parse", "HEAD"]).stdout)
            .expect("base oid utf8")
            .trim()
            .to_string();
        git(root, &["checkout", "-b", "topic", &base, "-q"]);
        fs::write(root.join("topic.txt"), b"topic-only\n").expect("write topic file");
        git(root, &["add", "topic.txt"]);
        git_with_identity(root, &["commit", "-m", "topic", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        fs::write(root.join("main.txt"), b"main-only\n").expect("write main file");
        git(root, &["add", "main.txt"]);
        git_with_identity(root, &["commit", "-m", "main", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

fn prepare_up_to_date_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("hello.txt"), b"base\n").expect("write base file");
        git(root, &["add", "hello.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        git(root, &["checkout", "-b", "topic", "-q"]);
        fs::write(root.join("topic.txt"), b"topic\n").expect("write topic file");
        git(root, &["add", "topic.txt"]);
        git_with_identity(root, &["commit", "-m", "topic", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        git(root, &["merge", "topic", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

#[test]
fn rebase_clean_matches_upstream_git() {
    let root = unique_temp_dir("rebase-clean");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_diverged_repos(&upstream, &rust);
        let args = ["rebase", "master"];
        let expected = run_output_with_identity("git", &upstream, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "status differed for clean rebase"
        );
        assert!(
            actual.status.success(),
            "sley rebase failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            run_output("git", &upstream, &["rev-parse", "HEAD"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
            "HEAD differed after clean rebase"
        );
        assert_eq!(
            run_output("git", &upstream, &["log", "--oneline"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "--oneline"]).stdout,
            "log order differed after clean rebase"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rebase_already_up_to_date_matches_upstream_git() {
    let root = unique_temp_dir("rebase-up-to-date");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_up_to_date_repos(&upstream, &rust);
        let args = ["rebase", "master"];
        let expected = run_output_with_identity("git", &upstream, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            run_output("git", &upstream, &["rev-parse", "HEAD"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
            "HEAD differed after up-to-date rebase"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}