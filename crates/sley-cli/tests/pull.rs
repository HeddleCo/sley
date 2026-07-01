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
    run_success(sley_testkit::oracle_git(), cwd, args);
}

fn git_with_identity(cwd: &Path, args: &[&str]) {
    let output = run_output_with_identity(sley_testkit::oracle_git(), cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepare_identity(root: &Path) {
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
}

fn prepare_pull_clone(upstream: &Path, clone: &Path) {
    git(clone, &["init", "-q", "-b", "master"]);
    prepare_identity(clone);
    fs::write(clone.join("hello.txt"), b"base\n").expect("write base file");
    git(clone, &["add", "hello.txt"]);
    git_with_identity(clone, &["commit", "-m", "base", "-q"]);
    let upstream_arg = upstream.to_str().expect("upstream path is utf8");
    git(clone, &["remote", "add", "origin", upstream_arg]);
    git(clone, &["fetch", "origin", "-q"]);
    git(
        clone,
        &["branch", "--set-upstream-to=origin/master", "master"],
    );
    git(clone, &["config", "pull.rebase", "false"]);
}

fn prepare_fast_forward_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("hello.txt"), b"base\n").expect("write base file");
    git(upstream, &["add", "hello.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    let base = String::from_utf8(
        run_output(sley_testkit::oracle_git(), upstream, &["rev-parse", "HEAD"]).stdout,
    )
    .expect("base oid utf8")
    .trim()
    .to_string();
    git(upstream, &["checkout", "-b", "topic", &base, "-q"]);
    fs::write(upstream.join("topic.txt"), b"topic\n").expect("write topic file");
    git(upstream, &["add", "topic.txt"]);
    git_with_identity(upstream, &["commit", "-m", "topic", "-q"]);
    git(upstream, &["checkout", "master", "-q"]);
    git(upstream, &["merge", "topic", "-q"]);
}

fn prepare_fast_forward_clone(upstream: &Path, clone: &Path) {
    prepare_pull_clone(upstream, clone);
}

fn prepare_three_way_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("shared.txt"), b"base\n").expect("write shared file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    fs::write(upstream.join("shared.txt"), b"upstream\n").expect("write upstream file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "upstream", "-q"]);
}

fn prepare_three_way_clone(upstream: &Path, clone: &Path) {
    git(clone, &["init", "-q", "-b", "master"]);
    prepare_identity(clone);
    fs::write(clone.join("shared.txt"), b"base\n").expect("write shared file");
    git(clone, &["add", "shared.txt"]);
    git_with_identity(clone, &["commit", "-m", "base", "-q"]);
    prepare_pull_clone(upstream, clone);
    fs::write(clone.join("local.txt"), b"local\n").expect("write local file");
    git(clone, &["add", "local.txt"]);
    git_with_identity(clone, &["commit", "-m", "local", "-q"]);
}

#[test]
fn pull_fast_forward_matches_upstream_git() {
    let root = unique_temp_dir("pull-fast-forward");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_fast_forward_upstream(&upstream);
    prepare_fast_forward_clone(&upstream, &expected);
    prepare_fast_forward_clone(&upstream, &actual);
    let args = ["pull"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull fast-forward"
    );
    assert!(
        actual_output.status.success(),
        "sley pull failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    let actual_stdout = String::from_utf8_lossy(&actual_output.stdout);
    assert!(
        actual_stdout.contains("Fast-forward"),
        "expected Fast-forward in output"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after fast-forward pull"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pull_three_way_clean_matches_upstream_git() {
    let root = unique_temp_dir("pull-three-way-clean");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_three_way_upstream(&upstream);
    prepare_three_way_clone(&upstream, &expected);
    prepare_three_way_clone(&upstream, &actual);
    let args = ["pull"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull three-way"
    );
    assert!(
        actual_output.status.success(),
        "sley pull failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&actual_output.stdout).contains("ort"),
        "expected ort merge summary in output"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after three-way pull"
    );
    let _ = fs::remove_dir_all(&root);
}
