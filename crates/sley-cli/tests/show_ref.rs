use std::fs;
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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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

#[test]
fn show_ref_exists_matches_upstream_git() {
    let root = unique_temp_dir("show-ref-exists");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        for args in [
            vec!["show-ref", "--exists"],
            vec!["show-ref", "--exists", "HEAD"],
            vec!["show-ref", "--exists", "--", "HEAD"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
        );
        let head_ref = String::from_utf8(run_success(
            sley_testkit::oracle_git(),
            &root,
            &["symbolic-ref", "HEAD"],
        ))
        .expect("HEAD ref is utf8")
        .trim()
        .to_string();
        for args in [
            vec!["show-ref", "--exists", head_ref.as_str()],
            vec!["show-ref", "--exists", "HEAD"],
            vec!["show-ref", "--exists", "HEAD", head_ref.as_str()],
            vec!["show-ref", "--exists", "refs/heads/missing"],
            vec!["show-ref", "--exists", "main"],
            vec!["show-ref", "--exists", "refs/heads/foo..bar"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn show_ref_head_matches_upstream_git() {
    let root = unique_temp_dir("show-ref-head");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
        );
        run_success(sley_testkit::oracle_git(), &root, &["tag", "v1.0"]);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "feature/topic"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "ann",
                "-m",
                "annotated",
            ],
        );

        for args in [
            vec!["show-ref", "--head"],
            vec!["show-ref", "--head", "--heads"],
            vec!["show-ref", "--head", "--branches"],
            vec!["show-ref", "--head", "--no-head", "--branches"],
            vec!["show-ref", "--head", "--tags"],
            vec!["show-ref", "--branches", "--tags"],
            vec!["show-ref", "--branches", "--no-branches"],
            vec!["show-ref", "--tags", "--no-tags"],
            vec!["show-ref", "--head", "--hash"],
            vec!["show-ref", "--head", "-s"],
            vec!["show-ref", "--head", "-s8"],
            vec!["show-ref", "--head", "-ds"],
            vec!["show-ref", "--head", "-ds8"],
            vec!["show-ref", "--head", "-dq"],
            vec!["show-ref", "--head", "-qd"],
            vec!["show-ref", "--head", "-dqs8"],
            vec!["show-ref", "--head", "-qds8"],
            vec!["show-ref", "--head", "--no-hash"],
            vec!["show-ref", "--hash=8", "--no-hash", "--heads"],
            vec!["show-ref", "-s8", "--no-hash", "--heads"],
            vec!["show-ref", "--no-hash", "--hash=8", "--heads"],
            vec!["show-ref", "--no-hash", "-s8", "--heads"],
            vec!["show-ref", "--head", "--abbrev=8", "--branches"],
            vec!["show-ref", "--abbrev=8", "--no-abbrev", "--branches"],
            vec!["show-ref", "--no-abbrev", "--abbrev=8", "--branches"],
            vec!["show-ref", "--head", "--dereference", "--tags"],
            vec!["show-ref", "--dereference", "--no-dereference", "--tags"],
            vec!["show-ref", "--no-dereference", "--dereference", "--tags"],
            vec!["show-ref", "--", "refs/heads/main"],
            vec!["show-ref", "main"],
            vec!["show-ref", "heads/main"],
            vec!["show-ref", "v1.0"],
            vec!["show-ref", "tags/v1.0"],
            vec!["show-ref", "topic"],
            vec!["show-ref", "feature/topic"],
            vec!["show-ref", "feature"],
            vec!["show-ref", "refs/remotes/origin/HEAD"],
            vec!["show-ref", "--", "refs/heads/missing"],
            vec!["show-ref", "refs/heads/missing"],
            vec!["show-ref", "--verify", "HEAD"],
            vec!["show-ref", "--verify", "--", "HEAD"],
            vec!["show-ref", "--verify", "refs/remotes/origin/HEAD"],
            vec!["show-ref", "--verify", "--hash", "refs/remotes/origin/HEAD"],
            vec![
                "show-ref",
                "--verify",
                "--",
                "refs/heads/main",
                "refs/tags/v1.0",
            ],
            vec!["show-ref", "--verify", "--hash", "HEAD"],
            vec!["show-ref", "--verify", "--quiet", "HEAD"],
            vec!["show-ref", "--verify", "--quiet", "--no-quiet", "HEAD"],
            vec!["show-ref", "--verify", "refs/heads/missing"],
            vec!["show-ref", "--verify", "--quiet", "refs/heads/missing"],
            vec![
                "show-ref",
                "--verify",
                "--quiet",
                "--no-quiet",
                "refs/heads/missing",
            ],
            vec!["show-ref", "--verify", "-q", "refs/heads/missing"],
            vec![
                "show-ref",
                "--verify",
                "--no-quiet",
                "--quiet",
                "refs/heads/missing",
            ],
            vec!["show-ref", "--verify", "--no-verify", "main"],
            vec!["show-ref", "--no-verify", "--verify", "refs/heads/main"],
            vec!["show-ref", "--exists", "--no-exists", "main"],
            vec!["show-ref", "--no-exists", "--exists", "refs/heads/main"],
            vec!["show-ref", "--verify", "--head", "HEAD", "refs/heads/main"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn show_ref_exclude_existing_matches_upstream_git() {
    let root = unique_temp_dir("show-ref-exclude-existing");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
        );
        run_success(sley_testkit::oracle_git(), &root, &["tag", "v1.0"]);

        let input = b"refs/heads/main\nrefs/heads/new\nabc refs/tags/v1.0\nabc refs/tags/new^{}\ninvalid..ref\nHEAD\n";
        for args in [
            vec!["show-ref", "--exclude-existing"],
            vec!["show-ref", "--exclude-existing=refs/heads/"],
        ] {
            let expected = run_with_stdin(sley_testkit::oracle_git(), &root, &args, input);
            let actual = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, input);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn show_ref_reftable_repository_matches_upstream_git() {
    let root = unique_temp_dir("show-ref-reftable");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "--ref-format=reftable", "-b", "main"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
        );
        run_success(sley_testkit::oracle_git(), &root, &["tag", "v1.0"]);
        run_success(sley_testkit::oracle_git(), &root, &["branch", "feature"]);

        for args in [
            vec!["show-ref"],
            vec!["show-ref", "--head"],
            vec!["show-ref", "--branches"],
            vec!["show-ref", "--tags"],
            vec!["show-ref", "--verify", "HEAD"],
            vec!["show-ref", "--verify", "refs/heads/main"],
            vec!["show-ref", "--exists", "refs/tags/v1.0"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
