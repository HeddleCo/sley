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

fn prepare_conflict_origin(origin: &Path) {
    git(origin, &["init", "-q", "-b", "master"]);
    prepare_identity(origin);
    fs::write(origin.join("conflict.txt"), b"base\n").expect("write base file");
    git(origin, &["add", "conflict.txt"]);
    git_with_identity(origin, &["commit", "-m", "base", "-q"]);
    fs::write(origin.join("conflict.txt"), b"remote\n").expect("write remote file");
    git(origin, &["add", "conflict.txt"]);
    git_with_identity(origin, &["commit", "-m", "remote", "-q"]);
}

fn prepare_conflict_clone(origin: &Path, clone: &Path) {
    git(clone, &["init", "-q", "-b", "master"]);
    prepare_identity(clone);
    fs::write(clone.join("conflict.txt"), b"base\n").expect("write base file");
    git(clone, &["add", "conflict.txt"]);
    git_with_identity(clone, &["commit", "-m", "base", "-q"]);
    let origin_arg = origin.to_str().expect("origin path is utf8");
    git(clone, &["remote", "add", "origin", origin_arg]);
    git(clone, &["fetch", "origin", "-q"]);
    git(clone, &["branch", "--set-upstream-to=origin/master", "master"]);
    git(clone, &["config", "pull.rebase", "false"]);
    fs::write(clone.join("conflict.txt"), b"local\n").expect("write local file");
    git(clone, &["add", "conflict.txt"]);
    git_with_identity(clone, &["commit", "-m", "local", "-q"]);
}

fn pre_pull_head(program: &str, root: &Path) -> String {
    String::from_utf8(run_output(program, root, &["rev-parse", "HEAD"]).stdout)
        .expect("pre-pull HEAD utf8")
        .trim()
        .to_string()
}

fn start_conflict_pull(program: &str, root: &Path) -> String {
    let pre_pull = pre_pull_head(program, root);
    let output = run_output_with_identity(program, root, &["pull", "origin", "master"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected conflict pull to exit 1\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join(".git/MERGE_HEAD").is_file(),
        "expected MERGE_HEAD after conflict pull in {}",
        root.display()
    );
    pre_pull
}

fn resolve_conflict(root: &Path) {
    fs::write(root.join("conflict.txt"), b"resolved\n").expect("write resolved file");
    git(root, &["add", "conflict.txt"]);
}

#[test]
fn pull_conflict_then_continue_matches_upstream_git() {
    let root = unique_temp_dir("pull-conflict-continue");
    let origin = root.join("origin");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&origin).expect("create origin repo");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_conflict_origin(&origin);
        prepare_conflict_clone(&origin, &upstream);
        prepare_conflict_clone(&origin, &rust);

        let upstream_pre_pull = start_conflict_pull("git", &upstream);
        let rust_pre_pull = start_conflict_pull(env!("CARGO_BIN_EXE_sley"), &rust);
        assert_eq!(upstream_pre_pull, rust_pre_pull, "pre-pull HEAD differed");

        resolve_conflict(&upstream);
        resolve_conflict(&rust);

        let args = ["merge", "--continue"];
        let expected = run_output_with_identity("git", &upstream, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !upstream.join(".git/MERGE_HEAD").is_file(),
            "upstream MERGE_HEAD should be removed"
        );
        assert!(
            !rust.join(".git/MERGE_HEAD").is_file(),
            "git-rs MERGE_HEAD should be removed"
        );
        assert!(
            !upstream.join(".git/MERGE_MSG").is_file(),
            "upstream MERGE_MSG should be removed"
        );
        assert!(
            !rust.join(".git/MERGE_MSG").is_file(),
            "git-rs MERGE_MSG should be removed"
        );
        assert_eq!(
            run_output("git", &upstream, &["rev-parse", "HEAD"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
            "HEAD differed after merge --continue"
        );
        assert_eq!(
            run_output("git", &upstream, &["log", "-1", "--format=%P"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "-1", "--format=%P"]).stdout,
            "merge commit parents differed"
        );
        assert_eq!(
            run_output("git", &upstream, &["log", "-1", "--format=%s"]).stdout,
            run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "-1", "--format=%s"]).stdout,
            "merge commit subject differed"
        );
        assert_eq!(
            fs::read(upstream.join("conflict.txt")).expect("read upstream conflict file"),
            fs::read(rust.join("conflict.txt")).expect("read rust conflict file"),
            "worktree content differed after merge --continue"
        );

        let upstream_parents =
            String::from_utf8(run_output("git", &upstream, &["log", "-1", "--format=%P"]).stdout)
                .expect("upstream parents utf8");
        assert!(
            upstream_parents
                .split_whitespace()
                .any(|parent| parent == upstream_pre_pull),
            "pre-pull HEAD should be one parent of the merge commit"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}