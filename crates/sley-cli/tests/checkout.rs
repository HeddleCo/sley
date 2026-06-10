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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(sley_testkit::oracle_git())
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00 +0000")
        .args([
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn prepare_repo(root: &Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("hello.txt"), b"base\n").expect("write base file");
    git(root, &["add", "hello.txt"]);
    run_with_identity(root, &["commit", "-m", "base", "-q"]);
    let base_oid = String::from_utf8(git(root, &["rev-parse", "HEAD"]))
        .expect("base oid utf8")
        .trim()
        .to_string();
    fs::write(root.join("hello.txt"), b"main\n").expect("write main file");
    git(root, &["add", "hello.txt"]);
    run_with_identity(root, &["commit", "-m", "main", "-q"]);
    base_oid
}

fn assert_same_output(actual: Output, expected: Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn assert_same_state(upstream: &Path, rust: &Path, expected_file: &[u8]) {
    assert_eq!(
        git(rust, &["branch", "--show-current"]),
        git(upstream, &["branch", "--show-current"]),
        "current branch differed"
    );
    assert_eq!(
        git(rust, &["rev-parse", "HEAD"]),
        git(upstream, &["rev-parse", "HEAD"]),
        "HEAD differed"
    );
    assert_eq!(
        fs::read(rust.join("hello.txt")).expect("read rust file"),
        expected_file,
        "worktree file differed"
    );
    assert_eq!(
        git(rust, &["status", "--short"]),
        git(upstream, &["status", "--short"]),
        "status differed"
    );
}

#[test]
fn checkout_branch_creation_and_quiet_match_upstream_git() {
    let root = unique_temp_dir("checkout-branch-create");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        let base_oid = prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["checkout", "-b", "topic", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["checkout", "-q", "--no-quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-q", "-b", "side"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-B", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = [
            "checkout",
            "--no-progress",
            "--no-guess",
            "--ignore-other-worktrees",
            "--no-ignore-other-worktrees",
            "--no-recurse-submodules",
            "main",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-B", "fresh", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["checkout", "-q", "-B", "quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn switch_branch_creation_and_force_create_match_upstream_git() {
    let root = unique_temp_dir("switch-branch-create");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        let base_oid = prepare_repo(&upstream);
        prepare_repo(&rust);

        git(&upstream, &["branch", "topic", base_oid.as_str()]);
        git(&rust, &["branch", "topic", base_oid.as_str()]);

        let args = ["switch", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["switch", "-q", "--no-quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-c", "side", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = [
            "switch",
            "--no-progress",
            "--no-guess",
            "--ignore-other-worktrees",
            "--no-ignore-other-worktrees",
            "--no-recurse-submodules",
            "main",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-C", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-q", "--create=quiet"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");
    };
    let _ = fs::remove_dir_all(&root);
}
