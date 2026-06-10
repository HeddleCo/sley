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

fn run(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_output(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_output(sley_testkit::oracle_git(), cwd, args)
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

fn commit_empty(cwd: &Path, message: &str) {
    run(
        sley_testkit::oracle_git(),
        cwd,
        &[
            "-c",
            "user.name=A U Thor",
            "-c",
            "user.email=author@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            message,
        ],
    );
}

fn rev_parse(cwd: &Path, rev: &str) -> String {
    let output = run_output(sley_testkit::oracle_git(), cwd, &["rev-parse", rev]);
    assert!(output.status.success(), "rev-parse {rev} failed");
    String::from_utf8(output.stdout)
        .expect("rev-parse output is utf8")
        .trim()
        .to_string()
}

#[test]
fn merge_base_two_commits_matches_upstream_git() {
    let root = unique_temp_dir("merge-base");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        commit_empty(&root, "base");
        let base = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-qb", "left"]);
        commit_empty(&root, "left");
        let left = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-qb", "right", &base]);
        commit_empty(&root, "right");
        let right = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-qb", "right-child"]);
        commit_empty(&root, "right child");
        let right_child = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-qb", "third", &base]);
        commit_empty(&root, "third");
        let third = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-q", "left"]);
        run(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=A U Thor",
                "-c",
                "user.email=author@example.com",
                "merge",
                "-q",
                "--no-ff",
                "right",
                "-m",
                "merge right",
            ],
        );
        let merge_right = rev_parse(&root, "HEAD");

        for args in [
            vec!["merge-base", &left, &right],
            vec!["merge-base", "--all", &left, &right],
            vec!["merge-base", &left, &right, &third],
            vec!["merge-base", "--all", &left, &right, &third],
            vec!["merge-base", &merge_right, &right, &third],
            vec!["merge-base", "--all", &merge_right, &right, &third],
            vec!["merge-base", &base, &left],
            vec!["merge-base", "--is-ancestor", &base, &left],
            vec!["merge-base", "--is-ancestor", &left, &base],
            vec!["merge-base", "--octopus", &left],
            vec!["merge-base", "--octopus", &left, &right],
            vec!["merge-base", "--octopus", &left, &right, &base],
            vec!["merge-base", "--all", "--octopus", &left, &right, &base],
            vec!["merge-base", "--octopus", "--all", &left, &right, &base],
            vec!["merge-base", "--independent", &left],
            vec!["merge-base", "--independent", &left, &base, &right],
            vec!["merge-base", "--independent", &right_child, &right, &left],
            vec!["merge-base", "--independent", &left, &left],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn merge_base_no_common_history_matches_upstream_git() {
    let root = unique_temp_dir("merge-base-unrelated");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        commit_empty(&root, "base");
        let first = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "--orphan", "unrelated", "-q"]);
        commit_empty(&root, "unrelated");
        let second = rev_parse(&root, "HEAD");

        let args = ["merge-base", first.as_str(), second.as_str()];
        let expected = git(&root, &args);
        let actual = git_rs(&root, &args);
        assert_same_output(actual, expected, &args);

        let args = ["merge-base", "--octopus", first.as_str(), second.as_str()];
        let expected = git(&root, &args);
        let actual = git_rs(&root, &args);
        assert_same_output(actual, expected, &args);

        let args = [
            "merge-base",
            "--independent",
            first.as_str(),
            second.as_str(),
        ];
        let expected = git(&root, &args);
        let actual = git_rs(&root, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn merge_base_fork_point_matches_upstream_git() {
    let root = unique_temp_dir("merge-base-fork-point");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        commit_empty(&root, "base");
        let base = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["branch", "-m", "main"]);
        commit_empty(&root, "upstream");
        let upstream = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-qb", "topic"]);
        commit_empty(&root, "topic");
        let topic = rev_parse(&root, "HEAD");
        run(sley_testkit::oracle_git(), &root, &["checkout", "-q", "main"]);
        run(sley_testkit::oracle_git(), &root, &["reset", "-q", "--hard", &base]);

        for args in [
            vec!["merge-base", "main", &topic],
            vec!["merge-base", "--fork-point", "main", &topic],
            vec!["merge-base", "--fork-point", "main"],
            vec!["merge-base", "--fork-point", "refs/heads/main", &topic],
            vec!["merge-base", "--fork-point", "main", &upstream],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
