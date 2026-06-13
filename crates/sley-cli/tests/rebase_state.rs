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
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
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

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout,
        expected.stdout,
        "stdout differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
    assert_eq!(
        actual.stderr,
        expected.stderr,
        "stderr differed for {args:?}\nactual:\n{}\nexpected:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
    );
}

fn prepare_identity(root: &Path) {
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
}

fn prepare_conflict_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("c.txt"), b"base\n").expect("write base file");
        git(root, &["add", "c.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        let base = String::from_utf8(
            run_output(sley_testkit::oracle_git(), root, &["rev-parse", "HEAD"]).stdout,
        )
        .expect("base oid utf8")
        .trim()
        .to_string();
        git(root, &["checkout", "-b", "topic", &base, "-q"]);
        fs::write(root.join("c.txt"), b"topic\n").expect("write topic file");
        git(root, &["add", "c.txt"]);
        git_with_identity(root, &["commit", "-m", "topic", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        fs::write(root.join("c.txt"), b"main\n").expect("write main file");
        git(root, &["add", "c.txt"]);
        git_with_identity(root, &["commit", "-m", "main", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

fn topic_head(program: &str, root: &Path) -> String {
    String::from_utf8(run_output(program, root, &["rev-parse", "topic"]).stdout)
        .expect("topic HEAD utf8")
        .trim()
        .to_string()
}

fn start_conflict_rebase(program: &str, root: &Path) {
    let output = run_output_with_identity(program, root, &["rebase", "master"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected conflict rebase to exit 1\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join(".git/rebase-merge").is_dir(),
        "expected rebase-merge after conflict rebase in {}",
        root.display()
    );
    assert!(
        root.join(".git/REBASE_HEAD").is_file(),
        "expected REBASE_HEAD after conflict rebase in {}",
        root.display()
    );
}

fn resolve_conflict(root: &Path) {
    fs::write(root.join("c.txt"), b"resolved\n").expect("write resolved file");
    git(root, &["add", "c.txt"]);
}

#[test]
fn rebase_conflict_then_abort_matches_upstream_git() {
    let root = unique_temp_dir("rebase-abort-conflict");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_conflict_repos(&upstream, &rust);
    let upstream_pre_rebase = topic_head(sley_testkit::oracle_git(), &upstream);
    let rust_pre_rebase = topic_head(env!("CARGO_BIN_EXE_sley"), &rust);
    assert_eq!(
        upstream_pre_rebase, rust_pre_rebase,
        "pre-rebase topic differed"
    );
    start_conflict_rebase(sley_testkit::oracle_git(), &upstream);
    start_conflict_rebase(env!("CARGO_BIN_EXE_sley"), &rust);

    let args = ["rebase", "--abort"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);

    assert!(
        !upstream.join(".git/rebase-merge").exists(),
        "upstream rebase-merge should be removed"
    );
    assert!(
        !rust.join(".git/rebase-merge").exists(),
        "git-rs rebase-merge should be removed"
    );
    assert!(
        !upstream.join(".git/REBASE_HEAD").is_file(),
        "upstream REBASE_HEAD should be removed"
    );
    assert!(
        !rust.join(".git/REBASE_HEAD").is_file(),
        "git-rs REBASE_HEAD should be removed"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after rebase --abort"
    );
    let upstream_post_abort_head = String::from_utf8(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"],
        )
        .stdout,
    )
    .expect("upstream post-abort HEAD utf8")
    .trim()
    .to_string();
    assert_eq!(
        upstream_pre_rebase, upstream_post_abort_head,
        "upstream HEAD should return to pre-rebase topic commit"
    );
    assert_eq!(
        fs::read(upstream.join("c.txt")).expect("read upstream c file"),
        fs::read(rust.join("c.txt")).expect("read rust c file"),
        "worktree content differed after rebase --abort"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["status", "--porcelain"]
        )
        .stdout,
        run_output(
            env!("CARGO_BIN_EXE_sley"),
            &rust,
            &["status", "--porcelain"]
        )
        .stdout,
        "status differed after rebase --abort"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["ls-files", "--unmerged"]
        )
        .stdout,
        run_output(
            env!("CARGO_BIN_EXE_sley"),
            &rust,
            &["ls-files", "--unmerged"]
        )
        .stdout,
        "unmerged index entries differed after rebase --abort"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_conflict_then_continue_matches_upstream_git() {
    let root = unique_temp_dir("rebase-continue-resolved");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_conflict_repos(&upstream, &rust);
    start_conflict_rebase(sley_testkit::oracle_git(), &upstream);
    start_conflict_rebase(env!("CARGO_BIN_EXE_sley"), &rust);
    resolve_conflict(&upstream);
    resolve_conflict(&rust);

    let args = ["rebase", "--continue"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);

    assert!(
        !upstream.join(".git/rebase-merge").exists(),
        "upstream rebase-merge should be removed"
    );
    assert!(
        !rust.join(".git/rebase-merge").exists(),
        "git-rs rebase-merge should be removed"
    );
    assert_eq!(
        upstream.join(".git/REBASE_HEAD").is_file(),
        rust.join(".git/REBASE_HEAD").is_file(),
        "REBASE_HEAD presence differed after rebase --continue"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after rebase --continue"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &upstream, &["log", "--oneline"]).stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "--oneline"]).stdout,
        "log order differed after rebase --continue"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "topic"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "topic"]).stdout,
        "topic branch differed after rebase --continue"
    );
    assert_eq!(
        fs::read(upstream.join("c.txt")).expect("read upstream c file"),
        fs::read(rust.join("c.txt")).expect("read rust c file"),
        "worktree content differed after rebase --continue"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_abort_without_rebase_fails() {
    let root = unique_temp_dir("rebase-abort-no-rebase");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    for repo in [&upstream, &rust] {
        git(repo, &["init", "-q", "-b", "master"]);
        prepare_identity(repo);
        fs::write(repo.join("hello.txt"), b"hello\n").expect("write hello file");
        git(repo, &["add", "hello.txt"]);
        git_with_identity(repo, &["commit", "-m", "init", "-q"]);
    }

    let args = ["rebase", "--abort"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);
    let _ = fs::remove_dir_all(&root);
}

fn prepare_multi_commit_conflict_repos(upstream: &Path, rust: &Path) {
    for root in [upstream, rust] {
        git(root, &["init", "-q", "-b", "master"]);
        prepare_identity(root);
        fs::write(root.join("a.txt"), b"base\n").expect("write base file");
        git(root, &["add", "a.txt"]);
        git_with_identity(root, &["commit", "-m", "base", "-q"]);
        let base = String::from_utf8(
            run_output(sley_testkit::oracle_git(), root, &["rev-parse", "HEAD"]).stdout,
        )
        .expect("base oid utf8")
        .trim()
        .to_string();
        git(root, &["checkout", "-b", "topic", &base, "-q"]);
        fs::write(root.join("a.txt"), b"commit1\n").expect("write commit1 file");
        git(root, &["add", "a.txt"]);
        git_with_identity(root, &["commit", "-m", "commit1", "-q"]);
        fs::write(root.join("b.txt"), b"commit2\n").expect("write commit2 file");
        git(root, &["add", "b.txt"]);
        git_with_identity(root, &["commit", "-m", "commit2", "-q"]);
        git(root, &["checkout", "master", "-q"]);
        fs::write(root.join("a.txt"), b"main\n").expect("write main file");
        git(root, &["add", "a.txt"]);
        git_with_identity(root, &["commit", "-m", "main", "-q"]);
        git(root, &["checkout", "topic", "-q"]);
    }
}

#[test]
fn rebase_conflict_then_skip_matches_upstream_git() {
    let root = unique_temp_dir("rebase-skip-conflict");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_conflict_repos(&upstream, &rust);
    start_conflict_rebase(sley_testkit::oracle_git(), &upstream);
    start_conflict_rebase(env!("CARGO_BIN_EXE_sley"), &rust);

    let args = ["rebase", "--skip"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);

    assert!(
        !upstream.join(".git/rebase-merge").exists(),
        "upstream rebase-merge should be removed"
    );
    assert!(
        !rust.join(".git/rebase-merge").exists(),
        "git-rs rebase-merge should be removed"
    );
    assert_eq!(
        upstream.join(".git/REBASE_HEAD").is_file(),
        rust.join(".git/REBASE_HEAD").is_file(),
        "REBASE_HEAD presence differed after rebase --skip"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after rebase --skip"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &upstream, &["log", "--oneline"]).stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "--oneline"]).stdout,
        "log order differed after rebase --skip"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "topic"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "topic"]).stdout,
        "topic branch differed after rebase --skip"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "master"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "master"]).stdout,
        "topic should match master after skipping sole topic commit"
    );
    assert_eq!(
        fs::read(upstream.join("c.txt")).expect("read upstream c file"),
        fs::read(rust.join("c.txt")).expect("read rust c file"),
        "worktree content differed after rebase --skip"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["status", "--porcelain"]
        )
        .stdout,
        run_output(
            env!("CARGO_BIN_EXE_sley"),
            &rust,
            &["status", "--porcelain"]
        )
        .stdout,
        "status differed after rebase --skip"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_multi_commit_conflict_then_skip_matches_upstream_git() {
    let root = unique_temp_dir("rebase-skip-multi-conflict");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_multi_commit_conflict_repos(&upstream, &rust);
    start_conflict_rebase(sley_testkit::oracle_git(), &upstream);
    start_conflict_rebase(env!("CARGO_BIN_EXE_sley"), &rust);

    let args = ["rebase", "--skip"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for multi-commit rebase --skip"
    );
    assert!(
        actual.status.success(),
        "sley rebase --skip failed: {}",
        String::from_utf8_lossy(&actual.stderr)
    );

    assert!(
        !upstream.join(".git/rebase-merge").exists(),
        "upstream rebase-merge should be removed"
    );
    assert!(
        !rust.join(".git/rebase-merge").exists(),
        "git-rs rebase-merge should be removed"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after multi-commit rebase --skip"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &upstream, &["log", "--oneline"]).stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["log", "--oneline"]).stdout,
        "log order differed after multi-commit rebase --skip"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "topic"]
        )
        .stdout,
        run_output(env!("CARGO_BIN_EXE_sley"), &rust, &["rev-parse", "topic"]).stdout,
        "topic branch differed after multi-commit rebase --skip"
    );
    assert_eq!(
        fs::read(upstream.join("a.txt")).expect("read upstream a file"),
        fs::read(rust.join("a.txt")).expect("read rust a file"),
        "a.txt content differed after multi-commit rebase --skip"
    );
    assert_eq!(
        fs::read(upstream.join("b.txt")).expect("read upstream b file"),
        fs::read(rust.join("b.txt")).expect("read rust b file"),
        "b.txt content differed after multi-commit rebase --skip"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_skip_without_rebase_fails() {
    let root = unique_temp_dir("rebase-skip-no-rebase");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    for repo in [&upstream, &rust] {
        git(repo, &["init", "-q", "-b", "master"]);
        prepare_identity(repo);
        fs::write(repo.join("hello.txt"), b"hello\n").expect("write hello file");
        git(repo, &["add", "hello.txt"]);
        git_with_identity(repo, &["commit", "-m", "init", "-q"]);
    }

    let args = ["rebase", "--skip"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rebase_continue_without_rebase_fails() {
    let root = unique_temp_dir("rebase-continue-no-rebase");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    for repo in [&upstream, &rust] {
        git(repo, &["init", "-q", "-b", "master"]);
        prepare_identity(repo);
        fs::write(repo.join("hello.txt"), b"hello\n").expect("write hello file");
        git(repo, &["add", "hello.txt"]);
        git_with_identity(repo, &["commit", "-m", "init", "-q"]);
    }

    let args = ["rebase", "--continue"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output_with_identity(env!("CARGO_BIN_EXE_sley"), &rust, &args);
    assert_same_output(actual, expected, &args);
    let _ = fs::remove_dir_all(&root);
}
