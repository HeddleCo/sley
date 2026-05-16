use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
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

fn run_output_with_editor(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .env("GIT_EDITOR", "true")
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
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

fn run_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(cwd)
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

fn run_with_identity_at(cwd: &Path, args: &[&str], timestamp: i64) -> Vec<u8> {
    let date = format!("@{timestamp} +0000");
    let output = Command::new("git")
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
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

fn branch_test_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    if args.iter().any(|arg| arg.contains("edit-description")) {
        run_output_with_editor(program, cwd, args)
    } else {
        run_output(program, cwd, args)
    }
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

#[test]
fn branch_delete_merged_matches_upstream_git() {
    let root = unique_temp_dir("branch-delete-merged");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "initial", "-q"]);
        let base_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("base oid utf8")
            .trim()
            .to_string();
        git(&root, &["branch", "merged", base_oid.as_str()]);
        git(&root, &["branch", "unmerged", base_oid.as_str()]);
        git(&root, &["checkout", "unmerged", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "unmerged", "-q"]);
        let unmerged_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("unmerged oid utf8")
            .trim()
            .to_string();
        git(&root, &["checkout", "main", "-q"]);

        for args in [
            vec!["branch", "-d", "unmerged"],
            vec!["branch", "--delete", "unmerged"],
            vec!["branch", "-d", "main"],
            vec!["branch", "-d", "missing"],
            vec!["branch", "-d"],
            vec!["branch", "-D", "main"],
            vec!["branch", "-D", "missing"],
            vec!["branch", "-D"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        for args in [
            vec!["branch", "--delete=topic"],
            vec!["branch", "--no-delete=topic"],
            vec!["branch", "--force=topic", "-d", "topic"],
            vec!["branch", "--no-force=topic", "-d", "topic"],
            vec!["branch", "--quiet=topic", "-d", "topic"],
            vec!["branch", "--no-quiet=topic", "-d", "topic"],
            vec!["branch", "--verbose=topic", "-d", "topic"],
            vec!["branch", "--no-verbose=topic", "-d", "topic"],
            vec!["branch", "--remotes=origin/topic", "-d", "origin/topic"],
            vec!["branch", "--all=topic", "-d", "topic"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        let args = ["branch", "-d", "merged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        git(&root, &["branch", "separator-merged", base_oid.as_str()]);
        let args = ["branch", "-d", "--", "separator-merged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "separator-merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        git(&root, &["branch", "merged", base_oid.as_str()]);
        let args = ["branch", "--delete", "merged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        let args = ["branch", "--delete", "--force", "unmerged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "unmerged", unmerged_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        git(&root, &["branch", "unmerged", unmerged_oid.as_str()]);

        let args = ["branch", "-D", "--", "unmerged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "unmerged", unmerged_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        git(&root, &["branch", "unmerged", unmerged_oid.as_str()]);

        let args = ["branch", "--delete", "--force", "--no-force", "unmerged"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        let args = ["branch", "--delete", "--no-force", "--force", "unmerged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "unmerged", unmerged_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        let args = [
            "branch",
            "--delete",
            "--no-delete",
            "cancelled-delete",
            base_oid.as_str(),
        ];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-D", "cancelled-delete"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "cancelled-delete"]),
            format!("{base_oid}\n").into_bytes()
        );

        git(&root, &["branch", "quiet-merged", base_oid.as_str()]);
        let args = ["branch", "-d", "--quiet", "quiet-merged"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "quiet-merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        git(&root, &["branch", "noisy-merged", base_oid.as_str()]);
        let args = [
            "branch",
            "--delete",
            "--quiet",
            "--no-quiet",
            "noisy-merged",
        ];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "noisy-merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        for (args, branch) in [
            (vec!["branch", "-dq", "cluster-quiet"], "cluster-quiet"),
            (
                vec!["branch", "-qd", "cluster-quiet-first"],
                "cluster-quiet-first",
            ),
            (
                vec!["branch", "-Dq", "cluster-force-quiet"],
                "cluster-force-quiet",
            ),
            (vec!["branch", "-dv", "cluster-verbose"], "cluster-verbose"),
            (
                vec!["branch", "-d", "--verbose", "long-verbose"],
                "long-verbose",
            ),
            (
                vec!["branch", "-d", "--no-verbose", "long-no-verbose"],
                "long-no-verbose",
            ),
        ] {
            git(&root, &["branch", branch, base_oid.as_str()]);
            let expected = run_output("git", &root, &args);
            git(&root, &["branch", branch, base_oid.as_str()]);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        for args in [
            vec!["branch", "-ad", "origin/topic"],
            vec!["branch", "-da", "origin/topic"],
            vec!["branch", "--all", "--delete", "origin/topic"],
            vec!["branch", "--delete", "--remotes", "--all", "origin/topic"],
            vec!["branch", "-D", "--", "-looks-like-option"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        git(&root, &["branch", "doomed", base_oid.as_str()]);
        let args = ["branch", "-D", "doomed", "missing"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "doomed", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        git(&root, &["branch", "merged", base_oid.as_str()]);
        let args = ["branch", "-d", "merged", "missing"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "merged", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_delete_remote_tracking_matches_upstream_git() {
    let root = unique_temp_dir("branch-delete-remote");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    let result = (|| {
        for repo in [&expected, &actual] {
            git(repo, &["init", "-q"]);
            run_with_identity_at(repo, &["commit", "--allow-empty", "-m", "initial", "-q"], 1);
        }

        for args in [
            vec!["branch", "-r", "-d", "origin/topic"],
            vec!["branch", "-rd", "origin/topic"],
            vec!["branch", "-dr", "origin/topic"],
            vec!["branch", "-r", "--delete", "origin/topic"],
            vec!["branch", "-d", "-r", "origin/topic"],
            vec!["branch", "-r", "-D", "origin/topic"],
            vec!["branch", "-rD", "origin/topic"],
            vec!["branch", "-Dr", "origin/topic"],
            vec!["branch", "-D", "-r", "origin/topic"],
            vec!["branch", "-rfD", "origin/topic"],
            vec!["branch", "-ard", "origin/topic"],
            vec!["branch", "-dra", "origin/topic"],
            vec!["branch", "--all", "--delete", "--remotes", "origin/topic"],
            vec!["branch", "--delete", "--all", "--remotes", "origin/topic"],
            vec!["branch", "--delete", "--remotes", "--all", "origin/topic"],
            vec!["branch", "-r", "-d", "-q", "origin/topic"],
            vec!["branch", "-r", "-d", "--", "origin/topic"],
            vec![
                "branch",
                "-r",
                "-d",
                "--quiet",
                "--no-quiet",
                "origin/topic",
            ],
            vec!["branch", "-r", "-d", "origin/missing"],
            vec!["branch", "-r", "-d"],
            vec!["branch", "-r", "-d", "refs/remotes/origin/topic"],
        ] {
            git(
                &expected,
                &["update-ref", "refs/remotes/origin/topic", "HEAD"],
            );
            git(
                &actual,
                &["update-ref", "refs/remotes/origin/topic", "HEAD"],
            );
            let expected_output = run_output("git", &expected, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_eq!(
                git(&actual, &["branch", "-r"]),
                git(&expected, &["branch", "-r"])
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_force_update_matches_upstream_git() {
    let root = unique_temp_dir("branch-force-update");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "initial", "-q"]);
        let base_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("base oid utf8")
            .trim()
            .to_string();
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "second", "-q"]);
        let head_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("head oid utf8")
            .trim()
            .to_string();

        let args = ["branch", "--"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        for args in [
            vec!["branch", "--", "--list"],
            vec!["branch", "--", "-bad"],
            vec!["branch", "--", "bad name"],
            vec!["branch", "-f", "--", "-bad"],
            vec!["branch", "--force", "--", "--list"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        for (args, branch, oid) in [
            (
                vec!["branch", "--", "separator-topic"],
                "separator-topic",
                head_oid.as_str(),
            ),
            (
                vec!["branch", "--", "separator-start", base_oid.as_str()],
                "separator-start",
                base_oid.as_str(),
            ),
            (
                vec![
                    "branch",
                    "--quiet",
                    "--",
                    "separator-quiet",
                    base_oid.as_str(),
                ],
                "separator-quiet",
                base_oid.as_str(),
            ),
        ] {
            let expected = run_output("git", &root, &args);
            git(&root, &["branch", "-D", branch]);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                git(&root, &["rev-parse", branch]),
                format!("{oid}\n").into_bytes()
            );
        }

        let args = ["branch", "--force", "topic", base_oid.as_str()];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-D", "topic"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "topic"]),
            format!("{base_oid}\n").into_bytes()
        );

        for (args, branch) in [
            (
                vec!["branch", "--quiet", "quiet-topic", base_oid.as_str()],
                "quiet-topic",
            ),
            (
                vec![
                    "branch",
                    "--quiet",
                    "--no-quiet",
                    "noisy-topic",
                    base_oid.as_str(),
                ],
                "noisy-topic",
            ),
            (
                vec![
                    "branch",
                    "--create-reflog",
                    "reflog-topic",
                    base_oid.as_str(),
                ],
                "reflog-topic",
            ),
            (
                vec![
                    "branch",
                    "--create-reflog",
                    "--no-create-reflog",
                    "no-reflog-topic",
                    base_oid.as_str(),
                ],
                "no-reflog-topic",
            ),
            (
                vec![
                    "branch",
                    "--force",
                    "--no-force",
                    "no-force-topic",
                    base_oid.as_str(),
                ],
                "no-force-topic",
            ),
        ] {
            let expected = run_output("git", &root, &args);
            git(&root, &["branch", "-D", branch]);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                git(&root, &["rev-parse", branch]),
                format!("{base_oid}\n").into_bytes()
            );
        }

        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let args = ["branch", "-f", "topic", head_oid.as_str()];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "topic"]),
            format!("{head_oid}\n").into_bytes()
        );

        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let args = ["branch", "--force", "--", "topic", head_oid.as_str()];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "topic"]),
            format!("{head_oid}\n").into_bytes()
        );

        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let args = [
            "branch",
            "--no-force",
            "--force",
            "topic",
            head_oid.as_str(),
        ];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-f", "topic", base_oid.as_str()]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "topic"]),
            format!("{head_oid}\n").into_bytes()
        );

        let args = [
            "branch",
            "--force",
            "--no-force",
            "topic",
            base_oid.as_str(),
        ];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        git(&root, &["checkout", "topic", "-q"]);
        let args = ["branch", "--force", "topic", base_oid.as_str()];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_upstream_config_matches_upstream_git() {
    let root = unique_temp_dir("branch-upstream-config");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "initial", "-q"]);
        git(&root, &["branch", "topic"]);
        git(&root, &["remote", "add", "origin", "."]);
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        for args in [
            vec!["branch", "--set-upstream-to=main", "topic"],
            vec!["branch", "-u", "main", "topic"],
            vec!["branch", "-u", "main", "--", "topic"],
            vec!["branch", "-umain", "topic"],
            vec!["branch", "--set-upstream-to", "origin/main", "topic"],
            vec!["branch", "--set-upstream-to", "main", "--", "topic"],
            vec!["branch", "-u", "refs/remotes/origin/main", "topic"],
            vec![
                "branch",
                "--no-set-upstream-to",
                "--set-upstream-to=main",
                "topic",
            ],
        ] {
            let expected = run_output("git", &root, &args);
            let _ = run_output("git", &root, &["branch", "--unset-upstream", "topic"]);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
            let remote = String::from_utf8(git(&root, &["config", "branch.topic.remote"]))
                .expect("remote utf8");
            let merge = String::from_utf8(git(&root, &["config", "branch.topic.merge"]))
                .expect("merge utf8");
            if args.iter().any(|arg| arg.contains("origin/main")) {
                assert_eq!(remote, "origin\n");
            } else {
                assert_eq!(remote, ".\n");
            }
            assert_eq!(merge, "refs/heads/main\n");
            let _ = run_output("git", &root, &["branch", "--unset-upstream", "topic"]);
        }

        git(&root, &["branch", "-u", "main", "topic"]);
        let args = ["branch", "--unset-upstream", "topic"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-u", "main", "topic"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            run_output(
                "git",
                &root,
                &["config", "--get-regexp", "^branch\\.topic\\."]
            )
            .status
            .code(),
            Some(1)
        );

        git(&root, &["branch", "-u", "main", "topic"]);
        let args = ["branch", "--unset-upstream", "--", "topic"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-u", "main", "topic"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);

        for args in [
            vec!["branch", "-u"],
            vec!["branch", "--set-upstream-to"],
            vec!["branch", "--no-set-upstream-to=main"],
            vec!["branch", "--unset-upstream=topic"],
            vec!["branch", "--no-unset-upstream=topic"],
            vec!["branch", "--set-upstream-to=main", "missing"],
            vec!["branch", "--set-upstream-to=main", "bad name"],
            vec!["branch", "-u", "main", "--", "--list"],
            vec!["branch", "-u", "", "topic"],
            vec!["branch", "-u", "bad name", "topic"],
            vec!["branch", "-u--bad", "topic"],
            vec!["branch", "--set-upstream-to=missing", "topic"],
            vec!["branch", "--set-upstream-to=bad name", "topic"],
            vec!["branch", "--set-upstream-to=main", "topic", "extra"],
            vec!["branch", "--unset-upstream", "missing"],
            vec!["branch", "--unset-upstream", "bad name"],
            vec!["branch", "--unset-upstream", "--", "--list"],
            vec!["branch", "--unset-upstream", "topic", "extra"],
            vec!["branch", "-u", "main", "main"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_create_tracking_matches_upstream_git() {
    let root = unique_temp_dir("branch-create-tracking");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    let result = (|| {
        for repo in [&expected, &actual] {
            git(repo, &["init", "-q"]);
            run_with_identity(repo, &["commit", "--allow-empty", "-m", "initial", "-q"]);
            git(repo, &["remote", "add", "origin", "."]);
            git(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
            git(repo, &["branch", "-u", "origin/main", "main"]);
        }

        for (args, branch, should_track) in [
            (
                vec!["branch", "--track", "track-direct", "origin/main"],
                "track-direct",
                true,
            ),
            (
                vec!["branch", "-t", "track-short", "origin/main"],
                "track-short",
                true,
            ),
            (
                vec!["branch", "--track=direct", "track-equals", "origin/main"],
                "track-equals",
                true,
            ),
            (
                vec!["branch", "--quiet", "--track", "track-quiet", "origin/main"],
                "track-quiet",
                true,
            ),
            (
                vec!["branch", "--track", "--", "track-separator", "origin/main"],
                "track-separator",
                true,
            ),
            (
                vec![
                    "branch",
                    "--track",
                    "--no-track",
                    "track-cancelled",
                    "origin/main",
                ],
                "track-cancelled",
                false,
            ),
            (
                vec![
                    "branch",
                    "--no-track",
                    "--track",
                    "track-restored",
                    "origin/main",
                ],
                "track-restored",
                true,
            ),
            (
                vec!["branch", "--track=inherit", "track-inherit", "main"],
                "track-inherit",
                true,
            ),
            (
                vec!["branch", "--no-recurse-submodules", "no-recurse"],
                "no-recurse",
                false,
            ),
            (
                vec![
                    "branch",
                    "--recurse-submodules",
                    "--no-recurse-submodules",
                    "recurse-reset",
                ],
                "recurse-reset",
                false,
            ),
            (
                vec!["branch", "--no-set-upstream", "legacy-no-set", "main"],
                "legacy-no-set",
                false,
            ),
            (
                vec![
                    "branch",
                    "--set-upstream",
                    "--no-set-upstream",
                    "legacy-reset",
                    "main",
                ],
                "legacy-reset",
                false,
            ),
            (
                vec!["branch", "--no-edit-description", "no-edit", "main"],
                "no-edit",
                false,
            ),
            (
                vec![
                    "branch",
                    "--edit-description",
                    "--no-edit-description",
                    "edit-reset",
                    "main",
                ],
                "edit-reset",
                false,
            ),
        ] {
            let expected_output = branch_test_output("git", &expected, &args);
            let actual_output = branch_test_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            let config_pattern = format!("^branch\\.{branch}\\.");
            let config_args = ["config", "--get-regexp", config_pattern.as_str()];
            let expected_config = run_output("git", &expected, &config_args);
            let actual_config = run_output("git", &actual, &config_args);
            let tracked = expected_config.status.success();
            assert_same_output(actual_config, expected_config, &config_args);
            assert_eq!(tracked, should_track);
        }

        let args = [
            "branch",
            "--track=inherit",
            "track-inherit-missing",
            "origin/main",
        ];
        let expected_output = branch_test_output("git", &expected, &args);
        let actual_output = branch_test_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);

        for args in [
            vec!["branch", "--recurse-submodules", "recurse-fatal"],
            vec![
                "branch",
                "--recurse-submodules",
                "--no-recurse-submodules",
                "--recurse-submodules",
                "recurse-restored",
            ],
            vec!["branch", "--recurse-submodules=on-demand", "recurse-value"],
            vec![
                "branch",
                "--no-recurse-submodules=never",
                "no-recurse-value",
            ],
            vec!["branch", "--set-upstream", "legacy-fatal", "main"],
            vec![
                "branch",
                "--no-set-upstream",
                "--set-upstream",
                "legacy-restored",
                "main",
            ],
            vec!["branch", "--set-upstream=main", "legacy-value"],
            vec!["branch", "--no-set-upstream=main", "legacy-no-value"],
            vec!["branch", "--edit-description", "edit-too", "main"],
            vec![
                "branch",
                "--no-edit-description",
                "--edit-description",
                "edit-restored",
                "main",
            ],
            vec!["branch", "--edit-description=main", "edit-value"],
            vec!["branch", "--no-edit-description=main", "edit-no-value"],
        ] {
            let expected_output = branch_test_output("git", &expected, &args);
            let actual_output = branch_test_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }

        for args in [
            vec!["branch", "--set-upstream"],
            vec!["branch", "--no-set-upstream", "--set-upstream"],
            vec!["branch", "--set-upstream", "--no-set-upstream"],
            vec!["branch", "--edit-description"],
            vec!["branch", "--edit-description", "main"],
            vec!["branch", "--no-edit-description", "--edit-description"],
            vec!["branch", "--edit-description", "--no-edit-description"],
        ] {
            let expected_output = branch_test_output("git", &expected, &args);
            let actual_output = branch_test_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }

        git(&expected, &["checkout", "--detach", "-q"]);
        git(&actual, &["checkout", "--detach", "-q"]);
        let args = ["branch", "--edit-description"];
        let expected_output = branch_test_output("git", &expected, &args);
        let actual_output = branch_test_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_rename_and_copy_match_upstream_git() {
    let root = unique_temp_dir("branch-rename-copy");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "initial", "-q"]);
        git(&root, &["branch", "topic"]);
        git(&root, &["branch", "target"]);
        git(&root, &["branch", "-u", "main", "topic"]);

        let args = ["branch", "-m", "topic", "renamed"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-D", "renamed"]);
        git(&root, &["branch", "topic"]);
        git(&root, &["branch", "-u", "main", "topic"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["rev-parse", "renamed"]),
            git(&root, &["rev-parse", "main"])
        );
        assert_eq!(
            git(&root, &["config", "branch.renamed.remote"]),
            b".\n".to_vec()
        );
        assert_eq!(
            git(&root, &["reflog", "show", "--format=%gs", "renamed"])[..],
            b"Branch: renamed refs/heads/topic to refs/heads/renamed\nbranch: Created from main\n"
                [..]
        );

        let args = ["branch", "-c", "renamed", "copied"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-D", "copied"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["config", "branch.copied.merge"]),
            b"refs/heads/main\n".to_vec()
        );
        assert_eq!(
            git(&root, &["reflog", "show", "--format=%gs", "copied"])[..],
            b"Branch: copied refs/heads/renamed to refs/heads/copied\nBranch: renamed refs/heads/topic to refs/heads/renamed\nbranch: Created from main\n"[..]
        );

        for (args, source, target) in [
            (
                vec!["branch", "-m", "--", "separator-move", "separator-renamed"],
                "separator-move",
                "separator-renamed",
            ),
            (
                vec!["branch", "-M", "--", "separator-force", "target"],
                "separator-force",
                "target",
            ),
            (
                vec!["branch", "-c", "--", "separator-copy", "separator-copied"],
                "separator-copy",
                "separator-copied",
            ),
            (
                vec!["branch", "-C", "--", "separator-copy-force", "target"],
                "separator-copy-force",
                "target",
            ),
        ] {
            git(&root, &["branch", "-f", source, "main"]);
            let expected = run_output("git", &root, &args);
            if target != "target" {
                git(&root, &["branch", "-D", target]);
            }
            git(&root, &["branch", "-f", source, "main"]);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                git(&root, &["rev-parse", target]),
                git(&root, &["rev-parse", "main"])
            );
        }

        git(&root, &["checkout", "renamed", "-q"]);
        let args = ["branch", "-m", "current-name"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-m", "current-name", "renamed"]);
        git(&root, &["checkout", "renamed", "-q"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["symbolic-ref", "HEAD"]),
            b"refs/heads/current-name\n".to_vec()
        );

        for args in [
            vec!["branch", "-m"],
            vec!["branch", "-m", "missing", "new"],
            vec!["branch", "-m", "current-name", "target"],
            vec!["branch", "-m", "a", "b", "c"],
            vec!["branch", "-c"],
            vec!["branch", "-c", "missing", "new-copy"],
            vec!["branch", "-c", "current-name", "target"],
            vec!["branch", "-c", "a", "b", "c"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        for args in [
            vec!["branch", "-m", "current-name", "bad name"],
            vec!["branch", "-M", "current-name", "bad name"],
            vec!["branch", "-c", "current-name", "bad name"],
            vec!["branch", "-C", "current-name", "bad name"],
            vec!["branch", "-m", "bad name", "new-name"],
            vec!["branch", "-c", "bad name", "new-copy"],
            vec!["branch", "-m", "--", "--list", "renamed-option"],
            vec!["branch", "-m", "--", "current-name", "--list"],
            vec!["branch", "-c", "--", "--list", "copied-option"],
            vec!["branch", "-c", "--", "current-name", "--list"],
            vec!["branch", "-m", "--force=x", "current-name", "new-name"],
            vec!["branch", "-m", "--no-force=x", "current-name", "new-name"],
            vec!["branch", "-m", "--quiet=x", "current-name", "new-name"],
            vec!["branch", "-m", "--no-quiet=x", "current-name", "new-name"],
            vec!["branch", "--move=x", "current-name", "new-name"],
            vec!["branch", "--no-move=x", "current-name", "new-name"],
            vec!["branch", "--copy=x", "current-name", "new-copy"],
            vec!["branch", "--no-copy=x", "current-name", "new-copy"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        let args = ["branch", "-M", "current-name", "target"];
        let expected = run_output("git", &root, &args);
        git(&root, &["branch", "-m", "target", "current-name"]);
        git(&root, &["branch", "target", "main"]);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["symbolic-ref", "HEAD"]),
            b"refs/heads/target\n".to_vec()
        );

        git(&root, &["branch", "source-copy"]);
        let args = ["branch", "-C", "source-copy", "target"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&root, &["symbolic-ref", "HEAD"]),
            b"refs/heads/target\n".to_vec()
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_verbose_listing_matches_upstream_git() {
    let root = unique_temp_dir("branch-verbose-listing");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "initial", "-q"]);
        let base_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("base oid utf8")
            .trim()
            .to_string();
        git(&root, &["branch", "feature"]);
        run_with_identity(&root, &["commit", "--allow-empty", "-m", "second", "-q"]);
        git(&root, &["branch", "ahead"]);
        git(&root, &["remote", "add", "origin", "."]);
        git(
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(
            &root,
            &["update-ref", "refs/remotes/origin/main", base_oid.as_str()],
        );
        git(&root, &["branch", "-u", "origin/main", "ahead"]);

        for args in [
            vec!["branch", "-v"],
            vec!["branch", "-vv"],
            vec!["branch", "--verbose"],
            vec!["branch", "--verbose", "--verbose"],
            vec!["branch", "-r", "-v"],
            vec!["branch", "-a", "-v"],
            vec!["branch", "--list", "-v", "f*"],
            vec!["branch", "--list", "-v", "--", "f*"],
            vec!["branch", "-v", "--list", "--", "f*"],
            vec!["branch", "-r", "--list", "-v", "--", "origin/*"],
            vec!["branch", "-a", "--list", "-v", "--", "origin/*"],
            vec!["branch", "--list", "-vv", "a*"],
            vec!["branch", "-v", "--no-verbose"],
            vec!["branch", "--no-verbose", "-v"],
            vec!["branch", "-v", "created-from-verbose"],
        ] {
            let expected = run_output("git", &root, &args);
            if args == ["branch", "-v", "created-from-verbose"] {
                let _ = run_output("git", &root, &["branch", "-D", "created-from-verbose"]);
            }
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn branch_list_patterns_match_upstream_git() {
    let root = unique_temp_dir("branch-list-patterns");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        run_with_identity_at(
            &root,
            &["commit", "--allow-empty", "-m", "initial", "-q"],
            1000,
        );
        let base_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("base oid utf8")
            .trim()
            .to_string();
        for branch in [
            "Feature/Bar",
            "feature/foo",
            "qa-1",
            "release/2026.05",
            "v1.9",
            "v1.10",
        ] {
            git(&root, &["branch", branch]);
        }
        run_with_identity_at(
            &root,
            &["commit", "--allow-empty", "-m", "main update", "-q"],
            3000,
        );
        let main_oid = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("main oid utf8")
            .trim()
            .to_string();
        git(&root, &["checkout", "feature/foo", "-q"]);
        run_with_identity_at(
            &root,
            &["commit", "--allow-empty", "-m", "feature update", "-q"],
            2000,
        );
        git(&root, &["checkout", "main", "-q"]);
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(
            &root,
            &["update-ref", "refs/remotes/origin/old", base_oid.as_str()],
        );
        git(
            &root,
            &["update-ref", "refs/remotes/origin/v1.9", base_oid.as_str()],
        );
        git(
            &root,
            &["update-ref", "refs/remotes/origin/v1.10", main_oid.as_str()],
        );
        git(
            &root,
            &["update-ref", "refs/remotes/Origin/Feature", "HEAD"],
        );
        git(
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        git(
            &root,
            &[
                "config",
                "remote.origin.push",
                "refs/heads/*:refs/heads/push/*",
            ],
        );
        git(&root, &["config", "branch.main.remote", "origin"]);
        git(&root, &["config", "branch.main.merge", "refs/heads/main"]);
        git(&root, &["config", "branch.feature/foo.remote", "origin"]);
        git(
            &root,
            &["config", "branch.feature/foo.merge", "refs/heads/old"],
        );

        for args in [
            vec!["branch", "--list", "f*"],
            vec!["branch", "--list", "release/*", "qa-?"],
            vec!["branch", "-r", "--list", "origin/*"],
            vec!["branch", "-a", "--list", "origin/*", "release/*"],
            vec!["branch", "--contains"],
            vec!["branch", "--contains", base_oid.as_str()],
            vec!["branch", "--contains", main_oid.as_str()],
            vec![
                "branch",
                "--list",
                "--contains",
                base_oid.as_str(),
                "feature/*",
            ],
            vec![
                "branch",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
            ],
            vec![
                "branch",
                "--no-contains",
                main_oid.as_str(),
                "--contains",
                base_oid.as_str(),
            ],
            vec!["branch", "--list", "--contains"],
            vec![
                "branch",
                "--list",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
            ],
            vec![
                "branch",
                "--list",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
                "feature/*",
            ],
            vec!["branch", "--points-at", main_oid.as_str()],
            vec!["branch", "--points-at", main_oid.as_str(), "--no-points-at"],
            vec!["branch", "--no-points-at", "--points-at", main_oid.as_str()],
            vec!["branch", "--list", "--no-points-at"],
            vec!["branch", "--list", "--no-points-at", "feature/*"],
            vec!["branch", "--no-points-at", "--list", "feature/*"],
            vec!["branch", "--list", "--points-at", main_oid.as_str()],
            vec!["branch", "--list", "--points-at", main_oid.as_str(), "m*"],
            vec![
                "branch",
                "--list",
                "--points-at",
                main_oid.as_str(),
                "--no-points-at",
            ],
            vec![
                "branch",
                "--list",
                "--no-points-at",
                "--points-at",
                main_oid.as_str(),
            ],
            vec!["branch", "-r", "--contains"],
            vec!["branch", "-r", "--contains", main_oid.as_str()],
            vec![
                "branch",
                "-r",
                "--list",
                "--contains",
                main_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "-r", "--no-points-at"],
            vec!["branch", "-r", "--list", "--no-points-at", "origin/*"],
            vec!["branch", "-r", "--points-at", main_oid.as_str()],
            vec![
                "branch",
                "-r",
                "--list",
                "--points-at",
                main_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--points-at",
                main_oid.as_str(),
                "--no-points-at",
            ],
            vec![
                "branch",
                "-r",
                "--no-points-at",
                "--points-at",
                main_oid.as_str(),
            ],
            vec!["branch", "-a", "--contains"],
            vec!["branch", "-a", "--contains", base_oid.as_str()],
            vec![
                "branch",
                "-a",
                "--list",
                "--contains",
                base_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--contains",
                base_oid.as_str(),
                "--no-contains",
                main_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "-a", "--no-points-at"],
            vec!["branch", "-a", "--list", "--no-points-at", "origin/*"],
            vec!["branch", "-a", "--points-at", main_oid.as_str()],
            vec![
                "branch",
                "-a",
                "--list",
                "--points-at",
                main_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--points-at",
                main_oid.as_str(),
                "--no-points-at",
            ],
            vec![
                "branch",
                "-a",
                "--no-points-at",
                "--points-at",
                main_oid.as_str(),
            ],
            vec!["branch", "--no-contains"],
            vec!["branch", "--no-contains", base_oid.as_str()],
            vec!["branch", "--no-contains", main_oid.as_str()],
            vec!["branch", "--list", "--no-contains"],
            vec![
                "branch",
                "--list",
                "--no-contains",
                main_oid.as_str(),
                "feature/*",
            ],
            vec!["branch", "-r", "--no-contains"],
            vec!["branch", "-r", "--no-contains", main_oid.as_str()],
            vec![
                "branch",
                "-r",
                "--list",
                "--no-contains",
                main_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "-a", "--no-contains"],
            vec!["branch", "-a", "--no-contains", main_oid.as_str()],
            vec![
                "branch",
                "-a",
                "--list",
                "--no-contains",
                main_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "--merged"],
            vec!["branch", "--merged", main_oid.as_str()],
            vec!["branch", "--list", "--merged", main_oid.as_str(), "m*"],
            vec!["branch", "--no-merged"],
            vec!["branch", "--no-merged", main_oid.as_str()],
            vec![
                "branch",
                "--list",
                "--no-merged",
                base_oid.as_str(),
                "feature/*",
            ],
            vec![
                "branch",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
            ],
            vec![
                "branch",
                "--no-merged",
                base_oid.as_str(),
                "--merged",
                main_oid.as_str(),
            ],
            vec![
                "branch",
                "--list",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
            ],
            vec![
                "branch",
                "--list",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
                "m*",
            ],
            vec!["branch", "-r", "--merged", main_oid.as_str()],
            vec![
                "branch",
                "-r",
                "--list",
                "--merged",
                main_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--no-merged",
                base_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "-a", "--merged", main_oid.as_str()],
            vec!["branch", "-a", "--no-merged", main_oid.as_str()],
            vec![
                "branch",
                "-a",
                "--list",
                "--merged",
                main_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--no-merged",
                base_oid.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--merged",
                main_oid.as_str(),
                "--no-merged",
                base_oid.as_str(),
                "origin/*",
            ],
            vec!["branch", "--no-color"],
            vec!["branch", "--color=never"],
            vec!["branch", "--color=auto"],
            vec!["branch", "--color"],
            vec!["branch", "--color=always"],
            vec!["branch", "--color", "--no-color"],
            vec!["branch", "--no-color", "--color"],
            vec!["branch", "--color=always", "--no-color"],
            vec!["branch", "--no-color", "--color=always"],
            vec!["branch", "--list", "--color"],
            vec!["branch", "--color", "--list"],
            vec!["branch", "--list", "--no-color"],
            vec!["branch", "--list", "--color=never"],
            vec!["branch", "--list", "--color=auto"],
            vec!["branch", "--no-color", "--list"],
            vec!["branch", "--color=never", "--list"],
            vec!["branch", "--color=auto", "--list"],
            vec!["branch", "--list", "--color", "--no-color"],
            vec!["branch", "--list", "--no-color", "--color"],
            vec!["branch", "--list", "--color", "feature/*"],
            vec!["branch", "--color=always", "--list", "Feature/*"],
            vec!["branch", "--list", "--no-color", "feature/*"],
            vec!["branch", "--color=never", "--list", "Feature/*"],
            vec!["branch", "--color=auto", "--list", "feature/*"],
            vec!["branch", "--list", "--color", "--no-color", "feature/*"],
            vec!["branch", "--list", "--no-color", "--color", "Feature/*"],
            vec![
                "branch",
                "--color=always",
                "--no-color",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--no-color",
                "--color=always",
                "--list",
                "Feature/*",
            ],
            vec!["branch", "-r", "--color"],
            vec!["branch", "--color", "-r"],
            vec!["branch", "-r", "--no-color"],
            vec!["branch", "-r", "--color=never"],
            vec!["branch", "-r", "--color=auto"],
            vec!["branch", "-r", "--color", "--no-color"],
            vec!["branch", "-r", "--no-color", "--color"],
            vec!["branch", "-r", "--color=always", "--no-color"],
            vec!["branch", "-r", "--no-color", "--color=always"],
            vec!["branch", "--no-color", "-r"],
            vec!["branch", "--color=never", "-r"],
            vec!["branch", "--color=auto", "-r"],
            vec!["branch", "-r", "--list", "--color", "origin/*"],
            vec!["branch", "-r", "--color=always", "--list", "Origin/*"],
            vec!["branch", "-r", "--list", "--no-color"],
            vec!["branch", "-r", "--list", "--color=never"],
            vec!["branch", "-r", "--list", "--color=auto"],
            vec!["branch", "-r", "--no-color", "--list"],
            vec!["branch", "-r", "--color=never", "--list"],
            vec!["branch", "-r", "--color=auto", "--list"],
            vec!["branch", "-r", "--list", "--no-color", "origin/*"],
            vec!["branch", "-r", "--color=never", "--list", "Origin/*"],
            vec!["branch", "-r", "--color=auto", "--list", "origin/*"],
            vec![
                "branch",
                "-r",
                "--list",
                "--color",
                "--no-color",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--no-color",
                "--color",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--color=always",
                "--no-color",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-color",
                "--color=always",
                "--list",
                "Origin/*",
            ],
            vec!["branch", "-a", "--color"],
            vec!["branch", "--color=always", "-a"],
            vec!["branch", "-a", "--no-color"],
            vec!["branch", "-a", "--color=never"],
            vec!["branch", "-a", "--color=auto"],
            vec!["branch", "-a", "--color", "--no-color"],
            vec!["branch", "-a", "--no-color", "--color"],
            vec!["branch", "-a", "--color=always", "--no-color"],
            vec!["branch", "-a", "--no-color", "--color=always"],
            vec!["branch", "--no-color", "-a"],
            vec!["branch", "--color=never", "-a"],
            vec!["branch", "--color=auto", "-a"],
            vec!["branch", "-a", "--list", "--color", "origin/*", "release/*"],
            vec!["branch", "-a", "--color=always", "--list", "Origin/*"],
            vec!["branch", "-a", "--list", "--no-color"],
            vec!["branch", "-a", "--list", "--color=never"],
            vec!["branch", "-a", "--list", "--color=auto"],
            vec!["branch", "-a", "--no-color", "--list"],
            vec!["branch", "-a", "--color=never", "--list"],
            vec!["branch", "-a", "--color=auto", "--list"],
            vec![
                "branch",
                "-a",
                "--list",
                "--no-color",
                "origin/*",
                "release/*",
            ],
            vec!["branch", "-a", "--color=never", "--list", "Origin/*"],
            vec!["branch", "-a", "--color=auto", "--list", "release/*"],
            vec![
                "branch",
                "-a",
                "--list",
                "--color",
                "--no-color",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--no-color",
                "--color",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--color=always",
                "--no-color",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-color",
                "--color=always",
                "--list",
                "feature/*",
            ],
            vec!["branch", "--no-column"],
            vec!["branch", "--column=auto"],
            vec!["branch", "--column=never"],
            vec!["branch", "--column=plain"],
            vec!["branch", "--no-column", "--column=plain"],
            vec!["branch", "--column=plain", "--no-column"],
            vec!["branch", "--list", "--no-column"],
            vec!["branch", "--list", "--column=auto"],
            vec!["branch", "--list", "--column=never"],
            vec!["branch", "--list", "--column=plain"],
            vec!["branch", "--column=auto", "--list", "feature/*"],
            vec!["branch", "--no-column", "--list", "feature/*"],
            vec!["branch", "--column=never", "--list", "feature/*"],
            vec!["branch", "--column=plain", "--list", "feature/*"],
            vec![
                "branch",
                "--no-column",
                "--column=plain",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--column=plain",
                "--no-column",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--no-column",
                "--column=plain",
                "feature/*",
            ],
            vec!["branch", "--list", "--abbrev"],
            vec!["branch", "--list", "--abbrev=12"],
            vec!["branch", "--list", "--no-abbrev"],
            vec!["branch", "--abbrev", "--list"],
            vec!["branch", "--abbrev=12", "--list"],
            vec!["branch", "--no-abbrev", "--list"],
            vec!["branch", "--list", "--abbrev", "feature/*"],
            vec!["branch", "--abbrev=12", "--list", "Feature/*"],
            vec!["branch", "--abbrev", "--no-abbrev"],
            vec!["branch", "--no-abbrev", "--abbrev=12"],
            vec![
                "branch",
                "--abbrev=12",
                "--no-abbrev",
                "--list",
                "feature/*",
            ],
            vec!["branch", "--no-abbrev", "--abbrev", "--list", "Feature/*"],
            vec![
                "branch",
                "--list",
                "--abbrev=12",
                "--no-abbrev",
                "feature/*",
            ],
            vec!["branch", "--list", "--sort=refname"],
            vec!["branch", "--list", "--sort=-refname"],
            vec!["branch", "--list", "--sort=version:refname"],
            vec!["branch", "--list", "--sort=-version:refname"],
            vec!["branch", "--list", "--sort=objectname"],
            vec!["branch", "--list", "--sort=-objectname"],
            vec!["branch", "--list", "--sort=objecttype"],
            vec!["branch", "--list", "--sort=-objecttype"],
            vec!["branch", "--list", "--sort=objectsize"],
            vec!["branch", "--list", "--sort=-objectsize"],
            vec!["branch", "--list", "--sort=committerdate"],
            vec!["branch", "--list", "--sort=-committerdate"],
            vec!["branch", "--list", "--sort", "authordate"],
            vec!["branch", "--list", "--sort", "-creatordate"],
            vec!["branch", "--list", "--sort=upstream"],
            vec!["branch", "--list", "--sort=-upstream"],
            vec!["branch", "--list", "--sort=push"],
            vec!["branch", "--list", "--sort=-push"],
            vec!["branch", "--sort=refname", "--list"],
            vec!["branch", "--sort=-refname", "--list"],
            vec!["branch", "--sort=v:refname", "--list"],
            vec!["branch", "--sort=-v:refname", "--list"],
            vec!["branch", "--sort=objectname", "--list"],
            vec!["branch", "--sort=-objectname", "--list"],
            vec!["branch", "--sort=objecttype", "--list"],
            vec!["branch", "--sort=-objecttype", "--list"],
            vec!["branch", "--sort=objectsize", "--list"],
            vec!["branch", "--sort=-objectsize", "--list"],
            vec!["branch", "--sort=committerdate", "--list"],
            vec!["branch", "--sort=-committerdate", "--list"],
            vec!["branch", "--sort", "authordate", "--list"],
            vec!["branch", "--sort", "-creatordate", "--list"],
            vec!["branch", "--sort=upstream", "--list"],
            vec!["branch", "--sort=-upstream", "--list"],
            vec!["branch", "--sort=push", "--list"],
            vec!["branch", "--sort=-push", "--list"],
            vec!["branch", "--list", "--no-sort"],
            vec!["branch", "--no-sort", "--list"],
            vec!["branch", "--list", "--sort=refname", "feature/*"],
            vec!["branch", "--list", "--sort=-refname", "feature/*"],
            vec!["branch", "--list", "--sort=version:refname", "v*"],
            vec!["branch", "--list", "--sort=-version:refname", "v*"],
            vec!["branch", "--list", "--sort=objectname", "feature/*"],
            vec!["branch", "--list", "--sort=-objectname", "feature/*"],
            vec!["branch", "--list", "--sort=objecttype", "feature/*"],
            vec!["branch", "--list", "--sort=-objecttype", "feature/*"],
            vec!["branch", "--list", "--sort=objectsize", "feature/*"],
            vec!["branch", "--list", "--sort=-objectsize", "feature/*"],
            vec!["branch", "--list", "--sort=committerdate", "feature/*"],
            vec!["branch", "--list", "--sort=-committerdate", "feature/*"],
            vec!["branch", "--list", "--sort", "authordate", "feature/*"],
            vec!["branch", "--list", "--sort", "-creatordate", "feature/*"],
            vec!["branch", "--list", "--sort=upstream", "feature/*"],
            vec!["branch", "--list", "--sort=-upstream", "feature/*"],
            vec!["branch", "--list", "--sort=push", "feature/*"],
            vec!["branch", "--list", "--sort=-push", "feature/*"],
            vec![
                "branch",
                "--sort=refname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-refname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=version:refname",
                "--no-sort",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--sort=-version:refname",
                "--no-sort",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--sort=objectname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-objectname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=objecttype",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-objecttype",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=objectsize",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-objectsize",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=committerdate",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-committerdate",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=upstream",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort=-upstream",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec!["branch", "--sort=push", "--no-sort", "--list", "feature/*"],
            vec!["branch", "--sort=-push", "--no-sort", "--list", "feature/*"],
            vec![
                "branch",
                "--no-sort",
                "--sort=refname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-refname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=version:refname",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-version:refname",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=objectname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-objectname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=objecttype",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-objecttype",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=objectsize",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-objectsize",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=committerdate",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-committerdate",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "authordate",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-creatordate",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=upstream",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort=-upstream",
                "--list",
                "Feature/*",
            ],
            vec!["branch", "--no-sort", "--sort=push", "--list", "Feature/*"],
            vec!["branch", "--no-sort", "--sort=-push", "--list", "Feature/*"],
            vec![
                "branch",
                "--list",
                "--sort=refname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=-refname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=version:refname",
                "--no-sort",
                "v*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=-version:refname",
                "--no-sort",
                "v*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=objectname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=-objectname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=objecttype",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=-objecttype",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=objectsize",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort=-objectsize",
                "--no-sort",
                "feature/*",
            ],
            vec!["branch", "--no-sort", "--list", "Feature/*"],
            vec!["branch", "--list", "--sort", "refname"],
            vec!["branch", "--list", "--sort", "-refname"],
            vec!["branch", "--list", "--sort", "version:refname"],
            vec!["branch", "--list", "--sort", "-version:refname"],
            vec!["branch", "--list", "--sort", "objectname"],
            vec!["branch", "--list", "--sort", "-objectname"],
            vec!["branch", "--list", "--sort", "objecttype"],
            vec!["branch", "--list", "--sort", "-objecttype"],
            vec!["branch", "--list", "--sort", "objectsize"],
            vec!["branch", "--list", "--sort", "-objectsize"],
            vec!["branch", "--list", "--sort", "upstream"],
            vec!["branch", "--list", "--sort", "-upstream"],
            vec!["branch", "--list", "--sort", "push"],
            vec!["branch", "--list", "--sort", "-push"],
            vec!["branch", "--sort", "refname", "--list"],
            vec!["branch", "--sort", "-refname", "--list"],
            vec!["branch", "--sort", "v:refname", "--list"],
            vec!["branch", "--sort", "-v:refname", "--list"],
            vec!["branch", "--sort", "objectname", "--list"],
            vec!["branch", "--sort", "-objectname", "--list"],
            vec!["branch", "--sort", "objecttype", "--list"],
            vec!["branch", "--sort", "-objecttype", "--list"],
            vec!["branch", "--sort", "objectsize", "--list"],
            vec!["branch", "--sort", "-objectsize", "--list"],
            vec!["branch", "--sort", "upstream", "--list"],
            vec!["branch", "--sort", "-upstream", "--list"],
            vec!["branch", "--sort", "push", "--list"],
            vec!["branch", "--sort", "-push", "--list"],
            vec!["branch", "--list", "--sort", "refname", "feature/*"],
            vec!["branch", "--list", "--sort", "-refname", "feature/*"],
            vec!["branch", "--list", "--sort", "version:refname", "v*"],
            vec!["branch", "--list", "--sort", "-version:refname", "v*"],
            vec!["branch", "--list", "--sort", "objectname", "feature/*"],
            vec!["branch", "--list", "--sort", "-objectname", "feature/*"],
            vec!["branch", "--list", "--sort", "objecttype", "feature/*"],
            vec!["branch", "--list", "--sort", "-objecttype", "feature/*"],
            vec!["branch", "--list", "--sort", "objectsize", "feature/*"],
            vec!["branch", "--list", "--sort", "-objectsize", "feature/*"],
            vec!["branch", "--list", "--sort", "upstream", "feature/*"],
            vec!["branch", "--list", "--sort", "-upstream", "feature/*"],
            vec!["branch", "--list", "--sort", "push", "feature/*"],
            vec!["branch", "--list", "--sort", "-push", "feature/*"],
            vec!["branch", "--sort", "refname", "--list", "Feature/*"],
            vec!["branch", "--sort", "-refname", "--list", "Feature/*"],
            vec!["branch", "--sort", "v:refname", "--list", "v*"],
            vec!["branch", "--sort", "-v:refname", "--list", "v*"],
            vec!["branch", "--sort", "objectname", "--list", "Feature/*"],
            vec!["branch", "--sort", "-objectname", "--list", "Feature/*"],
            vec!["branch", "--sort", "objecttype", "--list", "Feature/*"],
            vec!["branch", "--sort", "-objecttype", "--list", "Feature/*"],
            vec!["branch", "--sort", "objectsize", "--list", "Feature/*"],
            vec!["branch", "--sort", "-objectsize", "--list", "Feature/*"],
            vec!["branch", "--sort", "upstream", "--list", "Feature/*"],
            vec!["branch", "--sort", "-upstream", "--list", "Feature/*"],
            vec!["branch", "--sort", "push", "--list", "Feature/*"],
            vec!["branch", "--sort", "-push", "--list", "Feature/*"],
            vec![
                "branch",
                "--sort",
                "refname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "-refname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "version:refname",
                "--no-sort",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--sort",
                "-version:refname",
                "--no-sort",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--sort",
                "objectname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "-objectname",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "objecttype",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "-objecttype",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "objectsize",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--sort",
                "-objectsize",
                "--no-sort",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "refname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-refname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "version:refname",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-version:refname",
                "--list",
                "v*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "objectname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-objectname",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "objecttype",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-objecttype",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "objectsize",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--no-sort",
                "--sort",
                "-objectsize",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "refname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "version:refname",
                "--no-sort",
                "v*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "-version:refname",
                "--no-sort",
                "v*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "objectname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "-objectname",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "objecttype",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "-objecttype",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "objectsize",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "-objectsize",
                "--no-sort",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--sort",
                "-refname",
                "--no-sort",
                "feature/*",
            ],
            vec!["branch", "-r", "--no-column"],
            vec!["branch", "-r", "--column=auto"],
            vec!["branch", "-r", "--column=never"],
            vec!["branch", "-r", "--column=plain"],
            vec!["branch", "-r", "--no-column", "--column=plain"],
            vec!["branch", "-r", "--column=plain", "--no-column"],
            vec!["branch", "--no-column", "-r"],
            vec!["branch", "--column=auto", "-r"],
            vec!["branch", "--column=never", "-r"],
            vec!["branch", "--column=plain", "-r"],
            vec!["branch", "-r", "--list", "--no-column"],
            vec!["branch", "-r", "--list", "--column=auto"],
            vec!["branch", "-r", "--list", "--column=never"],
            vec!["branch", "-r", "--list", "--column=plain"],
            vec!["branch", "-r", "--column=auto", "--list", "origin/*"],
            vec!["branch", "-r", "--no-column", "--list", "Origin/*"],
            vec!["branch", "-r", "--column=never", "--list", "origin/*"],
            vec!["branch", "-r", "--column=plain", "--list", "Origin/*"],
            vec![
                "branch",
                "-r",
                "--no-column",
                "--column=plain",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--column=plain",
                "--no-column",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--no-column",
                "--column=plain",
                "origin/*",
            ],
            vec!["branch", "-r", "--list", "--sort", "refname"],
            vec!["branch", "-r", "--list", "--sort", "-refname"],
            vec!["branch", "-r", "--list", "--sort=-refname"],
            vec!["branch", "-r", "--list", "--sort", "version:refname"],
            vec!["branch", "-r", "--list", "--sort=-version:refname"],
            vec!["branch", "-r", "--list", "--sort", "objectname"],
            vec!["branch", "-r", "--list", "--sort=-objectname"],
            vec!["branch", "-r", "--list", "--sort", "objecttype"],
            vec!["branch", "-r", "--list", "--sort=-objecttype"],
            vec!["branch", "-r", "--list", "--sort", "objectsize"],
            vec!["branch", "-r", "--list", "--sort=-objectsize"],
            vec!["branch", "-r", "--list", "--sort", "upstream"],
            vec!["branch", "-r", "--list", "--sort=-upstream"],
            vec!["branch", "-r", "--list", "--sort", "push"],
            vec!["branch", "-r", "--list", "--sort=-push"],
            vec!["branch", "-r", "--sort", "refname", "--list"],
            vec!["branch", "-r", "--sort", "-refname", "--list"],
            vec!["branch", "-r", "--sort=-refname", "--list"],
            vec!["branch", "-r", "--sort", "v:refname", "--list"],
            vec!["branch", "-r", "--sort=-v:refname", "--list"],
            vec!["branch", "-r", "--sort", "objectname", "--list"],
            vec!["branch", "-r", "--sort=-objectname", "--list"],
            vec!["branch", "-r", "--sort", "objecttype", "--list"],
            vec!["branch", "-r", "--sort=-objecttype", "--list"],
            vec!["branch", "-r", "--sort", "objectsize", "--list"],
            vec!["branch", "-r", "--sort=-objectsize", "--list"],
            vec!["branch", "-r", "--sort", "upstream", "--list"],
            vec!["branch", "-r", "--sort=-upstream", "--list"],
            vec!["branch", "-r", "--sort", "push", "--list"],
            vec!["branch", "-r", "--sort=-push", "--list"],
            vec!["branch", "-r", "--list", "--sort", "refname", "origin/*"],
            vec!["branch", "-r", "--list", "--sort", "-refname", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-refname", "origin/*"],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "version:refname",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=-version:refname",
                "origin/v*",
            ],
            vec!["branch", "-r", "--list", "--sort", "objectname", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-objectname", "origin/*"],
            vec!["branch", "-r", "--list", "--sort", "objecttype", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-objecttype", "origin/*"],
            vec!["branch", "-r", "--list", "--sort", "objectsize", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-objectsize", "origin/*"],
            vec!["branch", "-r", "--list", "--sort", "upstream", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-upstream", "origin/*"],
            vec!["branch", "-r", "--list", "--sort", "push", "origin/*"],
            vec!["branch", "-r", "--list", "--sort=-push", "origin/*"],
            vec!["branch", "-r", "--sort", "refname", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "-refname", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-refname", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "v:refname", "--list", "origin/v*"],
            vec!["branch", "-r", "--sort=-v:refname", "--list", "origin/v*"],
            vec!["branch", "-r", "--sort", "objectname", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-objectname", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "objecttype", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-objecttype", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "objectsize", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-objectsize", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "upstream", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-upstream", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort", "push", "--list", "Origin/*"],
            vec!["branch", "-r", "--sort=-push", "--list", "Origin/*"],
            vec![
                "branch",
                "-r",
                "--sort=refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=-refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=-version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=objectname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=objecttype",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort=objectsize",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=refname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=-refname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=version:refname",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=-version:refname",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=-objectname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=-objecttype",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort=-objectsize",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=version:refname",
                "--no-sort",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=-version:refname",
                "--no-sort",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=objectname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=objecttype",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=objectsize",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort=-refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "-refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "-version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "objectname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "objecttype",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--sort",
                "objectsize",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "refname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "-refname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "version:refname",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "-version:refname",
                "--list",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "-objectname",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "-objecttype",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-sort",
                "--sort",
                "-objectsize",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "version:refname",
                "--no-sort",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "-version:refname",
                "--no-sort",
                "origin/v*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "-objectname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "-objecttype",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "-objectsize",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--sort",
                "-refname",
                "--no-sort",
                "origin/*",
            ],
            vec!["branch", "-a", "--no-column"],
            vec!["branch", "-a", "--column=auto"],
            vec!["branch", "-a", "--column=never"],
            vec!["branch", "-a", "--column=plain"],
            vec!["branch", "-a", "--no-column", "--column=plain"],
            vec!["branch", "-a", "--column=plain", "--no-column"],
            vec!["branch", "--no-column", "-a"],
            vec!["branch", "--column=auto", "-a"],
            vec!["branch", "--column=never", "-a"],
            vec!["branch", "--column=plain", "-a"],
            vec!["branch", "-a", "--list", "--no-column"],
            vec!["branch", "-a", "--list", "--column=auto"],
            vec!["branch", "-a", "--list", "--column=never"],
            vec!["branch", "-a", "--list", "--column=plain"],
            vec!["branch", "-a", "--column=auto", "--list", "origin/*"],
            vec!["branch", "-a", "--no-column", "--list", "release/*"],
            vec!["branch", "-a", "--column=never", "--list", "Origin/*"],
            vec!["branch", "-a", "--column=plain", "--list", "feature/*"],
            vec![
                "branch",
                "-a",
                "--no-column",
                "--column=plain",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--column=plain",
                "--no-column",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--no-column",
                "--column=plain",
                "origin/*",
            ],
            vec!["branch", "-a", "--list", "--sort", "refname"],
            vec!["branch", "-a", "--list", "--sort", "-refname"],
            vec!["branch", "-a", "--list", "--sort=-refname"],
            vec!["branch", "-a", "--list", "--sort", "version:refname"],
            vec!["branch", "-a", "--list", "--sort=-version:refname"],
            vec!["branch", "-a", "--list", "--sort", "objectname"],
            vec!["branch", "-a", "--list", "--sort=-objectname"],
            vec!["branch", "-a", "--list", "--sort", "objecttype"],
            vec!["branch", "-a", "--list", "--sort=-objecttype"],
            vec!["branch", "-a", "--list", "--sort", "objectsize"],
            vec!["branch", "-a", "--list", "--sort=-objectsize"],
            vec!["branch", "-a", "--list", "--sort", "upstream"],
            vec!["branch", "-a", "--list", "--sort=-upstream"],
            vec!["branch", "-a", "--list", "--sort", "push"],
            vec!["branch", "-a", "--list", "--sort=-push"],
            vec!["branch", "-a", "--sort", "refname", "--list"],
            vec!["branch", "-a", "--sort", "-refname", "--list"],
            vec!["branch", "-a", "--sort=-refname", "--list"],
            vec!["branch", "-a", "--sort", "v:refname", "--list"],
            vec!["branch", "-a", "--sort=-v:refname", "--list"],
            vec!["branch", "-a", "--sort", "objectname", "--list"],
            vec!["branch", "-a", "--sort=-objectname", "--list"],
            vec!["branch", "-a", "--sort", "objecttype", "--list"],
            vec!["branch", "-a", "--sort=-objecttype", "--list"],
            vec!["branch", "-a", "--sort", "objectsize", "--list"],
            vec!["branch", "-a", "--sort=-objectsize", "--list"],
            vec!["branch", "-a", "--sort", "upstream", "--list"],
            vec!["branch", "-a", "--sort=-upstream", "--list"],
            vec!["branch", "-a", "--sort", "push", "--list"],
            vec!["branch", "-a", "--sort=-push", "--list"],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "refname",
                "origin/*",
                "release/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-refname",
                "origin/*",
                "release/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-refname",
                "origin/*",
                "release/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "version:refname",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-version:refname",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "objectname",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-objectname",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "objecttype",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-objecttype",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "objectsize",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-objectsize",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "upstream",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-upstream",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "push",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-push",
                "origin/*",
                "feature/*",
            ],
            vec!["branch", "-a", "--sort", "refname", "--list", "feature/*"],
            vec!["branch", "-a", "--sort", "-refname", "--list", "feature/*"],
            vec!["branch", "-a", "--sort=-refname", "--list", "feature/*"],
            vec![
                "branch",
                "-a",
                "--sort",
                "v:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objectname",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-objectname",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objecttype",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-objecttype",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objectsize",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-objectsize",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "upstream",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-upstream",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "push",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-push",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-v:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=-version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=objectname",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=objecttype",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort=objectsize",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=refname",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=-refname",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=version:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=-version:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=-objectname",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=-objecttype",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort=-objectsize",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=version:refname",
                "--no-sort",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-version:refname",
                "--no-sort",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=objectname",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=objecttype",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=objectsize",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort=-refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "-refname",
                "--no-sort",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "-version:refname",
                "--no-sort",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objectname",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objecttype",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--sort",
                "objectsize",
                "--no-sort",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "refname",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "-refname",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "version:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "-version:refname",
                "--list",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "-objectname",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "-objecttype",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-sort",
                "--sort",
                "-objectsize",
                "--list",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "refname",
                "--no-sort",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "version:refname",
                "--no-sort",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-version:refname",
                "--no-sort",
                "origin/v*",
                "v*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-objectname",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-objecttype",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-objectsize",
                "--no-sort",
                "origin/*",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--sort",
                "-refname",
                "--no-sort",
                "origin/*",
            ],
            vec!["branch", "--abbrev"],
            vec!["branch", "--abbrev=12"],
            vec!["branch", "--no-abbrev"],
            vec!["branch", "-r", "--abbrev"],
            vec!["branch", "-r", "--abbrev=12"],
            vec!["branch", "-r", "--no-abbrev"],
            vec!["branch", "-r", "--abbrev", "--no-abbrev"],
            vec!["branch", "-r", "--no-abbrev", "--abbrev=12"],
            vec!["branch", "--abbrev", "-r"],
            vec!["branch", "--abbrev=12", "-r"],
            vec!["branch", "--no-abbrev", "-r"],
            vec![
                "branch",
                "-r",
                "--abbrev=12",
                "--no-abbrev",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-abbrev",
                "--abbrev",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--abbrev=12",
                "--no-abbrev",
                "origin/*",
            ],
            vec!["branch", "-a", "--abbrev"],
            vec!["branch", "-a", "--abbrev=12"],
            vec!["branch", "-a", "--no-abbrev"],
            vec!["branch", "-a", "--abbrev", "--no-abbrev"],
            vec!["branch", "-a", "--no-abbrev", "--abbrev=12"],
            vec!["branch", "--abbrev", "-a"],
            vec!["branch", "--abbrev=12", "-a"],
            vec!["branch", "--no-abbrev", "-a"],
            vec![
                "branch",
                "-a",
                "--abbrev=12",
                "--no-abbrev",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-abbrev",
                "--abbrev",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--abbrev=12",
                "--no-abbrev",
                "origin/*",
            ],
            vec!["branch", "--sort=refname"],
            vec!["branch", "--sort=-refname"],
            vec!["branch", "--sort=version:refname"],
            vec!["branch", "--sort=-version:refname"],
            vec!["branch", "--sort=objectname"],
            vec!["branch", "--sort=-objectname"],
            vec!["branch", "--sort=objecttype"],
            vec!["branch", "--sort=-objecttype"],
            vec!["branch", "--sort=objectsize"],
            vec!["branch", "--sort=-objectsize"],
            vec!["branch", "--sort=committerdate"],
            vec!["branch", "--sort=-committerdate"],
            vec!["branch", "--sort", "authordate"],
            vec!["branch", "--sort", "-creatordate"],
            vec!["branch", "--sort=upstream"],
            vec!["branch", "--sort=-upstream"],
            vec!["branch", "--sort=push"],
            vec!["branch", "--sort=-push"],
            vec!["branch", "--sort", "refname"],
            vec!["branch", "--sort", "-refname"],
            vec!["branch", "--sort", "v:refname"],
            vec!["branch", "--sort", "-v:refname"],
            vec!["branch", "--sort", "objectname"],
            vec!["branch", "--sort", "-objectname"],
            vec!["branch", "--sort", "objecttype"],
            vec!["branch", "--sort", "-objecttype"],
            vec!["branch", "--sort", "objectsize"],
            vec!["branch", "--sort", "-objectsize"],
            vec!["branch", "--sort", "upstream"],
            vec!["branch", "--sort", "-upstream"],
            vec!["branch", "--sort", "push"],
            vec!["branch", "--sort", "-push"],
            vec!["branch", "--no-sort"],
            vec!["branch", "--sort=refname", "--no-sort"],
            vec!["branch", "--sort=-refname", "--no-sort"],
            vec!["branch", "--sort=version:refname", "--no-sort"],
            vec!["branch", "--sort=-version:refname", "--no-sort"],
            vec!["branch", "--sort=objectname", "--no-sort"],
            vec!["branch", "--sort=-objectname", "--no-sort"],
            vec!["branch", "--sort=objecttype", "--no-sort"],
            vec!["branch", "--sort=-objecttype", "--no-sort"],
            vec!["branch", "--sort=objectsize", "--no-sort"],
            vec!["branch", "--sort=-objectsize", "--no-sort"],
            vec!["branch", "--sort=committerdate", "--no-sort"],
            vec!["branch", "--sort=-committerdate", "--no-sort"],
            vec!["branch", "--sort=upstream", "--no-sort"],
            vec!["branch", "--sort=-upstream", "--no-sort"],
            vec!["branch", "--sort=push", "--no-sort"],
            vec!["branch", "--sort=-push", "--no-sort"],
            vec!["branch", "--no-sort", "--sort=refname"],
            vec!["branch", "--no-sort", "--sort=-refname"],
            vec!["branch", "--no-sort", "--sort=version:refname"],
            vec!["branch", "--no-sort", "--sort=-version:refname"],
            vec!["branch", "--no-sort", "--sort=objectname"],
            vec!["branch", "--no-sort", "--sort=-objectname"],
            vec!["branch", "--no-sort", "--sort=objecttype"],
            vec!["branch", "--no-sort", "--sort=-objecttype"],
            vec!["branch", "--no-sort", "--sort=objectsize"],
            vec!["branch", "--no-sort", "--sort=-objectsize"],
            vec!["branch", "--no-sort", "--sort=committerdate"],
            vec!["branch", "--no-sort", "--sort=-committerdate"],
            vec!["branch", "--no-sort", "--sort=upstream"],
            vec!["branch", "--no-sort", "--sort=-upstream"],
            vec!["branch", "--no-sort", "--sort=push"],
            vec!["branch", "--no-sort", "--sort=-push"],
            vec!["branch", "--sort", "refname", "--no-sort"],
            vec!["branch", "--sort", "-refname", "--no-sort"],
            vec!["branch", "--sort", "version:refname", "--no-sort"],
            vec!["branch", "--sort", "-version:refname", "--no-sort"],
            vec!["branch", "--sort", "objectname", "--no-sort"],
            vec!["branch", "--sort", "-objectname", "--no-sort"],
            vec!["branch", "--sort", "objecttype", "--no-sort"],
            vec!["branch", "--sort", "-objecttype", "--no-sort"],
            vec!["branch", "--sort", "objectsize", "--no-sort"],
            vec!["branch", "--sort", "-objectsize", "--no-sort"],
            vec!["branch", "--sort", "authordate", "--no-sort"],
            vec!["branch", "--sort", "-creatordate", "--no-sort"],
            vec!["branch", "--sort", "upstream", "--no-sort"],
            vec!["branch", "--sort", "-upstream", "--no-sort"],
            vec!["branch", "--sort", "push", "--no-sort"],
            vec!["branch", "--sort", "-push", "--no-sort"],
            vec!["branch", "--no-sort", "--sort", "refname"],
            vec!["branch", "--no-sort", "--sort", "-refname"],
            vec!["branch", "--no-sort", "--sort", "version:refname"],
            vec!["branch", "--no-sort", "--sort", "-version:refname"],
            vec!["branch", "--no-sort", "--sort", "objectname"],
            vec!["branch", "--no-sort", "--sort", "-objectname"],
            vec!["branch", "--no-sort", "--sort", "objecttype"],
            vec!["branch", "--no-sort", "--sort", "-objecttype"],
            vec!["branch", "--no-sort", "--sort", "objectsize"],
            vec!["branch", "--no-sort", "--sort", "-objectsize"],
            vec!["branch", "--no-sort", "--sort", "authordate"],
            vec!["branch", "--no-sort", "--sort", "-creatordate"],
            vec!["branch", "--no-sort", "--sort", "upstream"],
            vec!["branch", "--no-sort", "--sort", "-upstream"],
            vec!["branch", "--no-sort", "--sort", "push"],
            vec!["branch", "--no-sort", "--sort", "-push"],
            vec!["branch", "-r", "--sort=refname"],
            vec!["branch", "-r", "--sort=-refname"],
            vec!["branch", "-r", "--sort=version:refname"],
            vec!["branch", "-r", "--sort=-version:refname"],
            vec!["branch", "-r", "--sort=objectname"],
            vec!["branch", "-r", "--sort=-objectname"],
            vec!["branch", "-r", "--sort=objecttype"],
            vec!["branch", "-r", "--sort=-objecttype"],
            vec!["branch", "-r", "--sort=objectsize"],
            vec!["branch", "-r", "--sort=-objectsize"],
            vec!["branch", "-r", "--sort=committerdate"],
            vec!["branch", "-r", "--sort=-committerdate"],
            vec!["branch", "-r", "--sort", "authordate"],
            vec!["branch", "-r", "--sort", "-creatordate"],
            vec!["branch", "-r", "--sort=upstream"],
            vec!["branch", "-r", "--sort=-upstream"],
            vec!["branch", "-r", "--sort=push"],
            vec!["branch", "-r", "--sort=-push"],
            vec!["branch", "-r", "--sort", "refname"],
            vec!["branch", "-r", "--sort", "-refname"],
            vec!["branch", "-r", "--sort", "v:refname"],
            vec!["branch", "-r", "--sort", "-v:refname"],
            vec!["branch", "-r", "--sort", "objectname"],
            vec!["branch", "-r", "--sort", "-objectname"],
            vec!["branch", "-r", "--sort", "objecttype"],
            vec!["branch", "-r", "--sort", "-objecttype"],
            vec!["branch", "-r", "--sort", "objectsize"],
            vec!["branch", "-r", "--sort", "-objectsize"],
            vec!["branch", "-r", "--sort", "upstream"],
            vec!["branch", "-r", "--sort", "-upstream"],
            vec!["branch", "-r", "--sort", "push"],
            vec!["branch", "-r", "--sort", "-push"],
            vec!["branch", "-r", "--no-sort"],
            vec!["branch", "-r", "--sort=refname", "--no-sort"],
            vec!["branch", "-r", "--sort=-refname", "--no-sort"],
            vec!["branch", "-r", "--sort=version:refname", "--no-sort"],
            vec!["branch", "-r", "--sort=-version:refname", "--no-sort"],
            vec!["branch", "-r", "--sort=objectname", "--no-sort"],
            vec!["branch", "-r", "--sort=-objectname", "--no-sort"],
            vec!["branch", "-r", "--sort=objecttype", "--no-sort"],
            vec!["branch", "-r", "--sort=-objecttype", "--no-sort"],
            vec!["branch", "-r", "--sort=objectsize", "--no-sort"],
            vec!["branch", "-r", "--sort=-objectsize", "--no-sort"],
            vec!["branch", "-r", "--sort=committerdate", "--no-sort"],
            vec!["branch", "-r", "--sort=-committerdate", "--no-sort"],
            vec!["branch", "-r", "--sort=upstream", "--no-sort"],
            vec!["branch", "-r", "--sort=-upstream", "--no-sort"],
            vec!["branch", "-r", "--sort=push", "--no-sort"],
            vec!["branch", "-r", "--sort=-push", "--no-sort"],
            vec!["branch", "-r", "--no-sort", "--sort=refname"],
            vec!["branch", "-r", "--no-sort", "--sort=-refname"],
            vec!["branch", "-r", "--no-sort", "--sort=version:refname"],
            vec!["branch", "-r", "--no-sort", "--sort=-version:refname"],
            vec!["branch", "-r", "--no-sort", "--sort=objectname"],
            vec!["branch", "-r", "--no-sort", "--sort=-objectname"],
            vec!["branch", "-r", "--no-sort", "--sort=objecttype"],
            vec!["branch", "-r", "--no-sort", "--sort=-objecttype"],
            vec!["branch", "-r", "--no-sort", "--sort=objectsize"],
            vec!["branch", "-r", "--no-sort", "--sort=-objectsize"],
            vec!["branch", "-r", "--no-sort", "--sort=committerdate"],
            vec!["branch", "-r", "--no-sort", "--sort=-committerdate"],
            vec!["branch", "-r", "--no-sort", "--sort=upstream"],
            vec!["branch", "-r", "--no-sort", "--sort=-upstream"],
            vec!["branch", "-r", "--no-sort", "--sort=push"],
            vec!["branch", "-r", "--no-sort", "--sort=-push"],
            vec!["branch", "-r", "--sort", "refname", "--no-sort"],
            vec!["branch", "-r", "--sort", "-refname", "--no-sort"],
            vec!["branch", "-r", "--sort", "version:refname", "--no-sort"],
            vec!["branch", "-r", "--sort", "-version:refname", "--no-sort"],
            vec!["branch", "-r", "--sort", "objectname", "--no-sort"],
            vec!["branch", "-r", "--sort", "-objectname", "--no-sort"],
            vec!["branch", "-r", "--sort", "objecttype", "--no-sort"],
            vec!["branch", "-r", "--sort", "-objecttype", "--no-sort"],
            vec!["branch", "-r", "--sort", "objectsize", "--no-sort"],
            vec!["branch", "-r", "--sort", "-objectsize", "--no-sort"],
            vec!["branch", "-r", "--sort", "authordate", "--no-sort"],
            vec!["branch", "-r", "--sort", "-creatordate", "--no-sort"],
            vec!["branch", "-r", "--sort", "upstream", "--no-sort"],
            vec!["branch", "-r", "--sort", "-upstream", "--no-sort"],
            vec!["branch", "-r", "--sort", "push", "--no-sort"],
            vec!["branch", "-r", "--sort", "-push", "--no-sort"],
            vec!["branch", "-r", "--no-sort", "--sort", "refname"],
            vec!["branch", "-r", "--no-sort", "--sort", "-refname"],
            vec!["branch", "-r", "--no-sort", "--sort", "version:refname"],
            vec!["branch", "-r", "--no-sort", "--sort", "-version:refname"],
            vec!["branch", "-r", "--no-sort", "--sort", "objectname"],
            vec!["branch", "-r", "--no-sort", "--sort", "-objectname"],
            vec!["branch", "-r", "--no-sort", "--sort", "objecttype"],
            vec!["branch", "-r", "--no-sort", "--sort", "-objecttype"],
            vec!["branch", "-r", "--no-sort", "--sort", "objectsize"],
            vec!["branch", "-r", "--no-sort", "--sort", "-objectsize"],
            vec!["branch", "-r", "--no-sort", "--sort", "authordate"],
            vec!["branch", "-r", "--no-sort", "--sort", "-creatordate"],
            vec!["branch", "-r", "--no-sort", "--sort", "upstream"],
            vec!["branch", "-r", "--no-sort", "--sort", "-upstream"],
            vec!["branch", "-r", "--no-sort", "--sort", "push"],
            vec!["branch", "-r", "--no-sort", "--sort", "-push"],
            vec!["branch", "--sort=refname", "-r"],
            vec!["branch", "--sort=-refname", "-r"],
            vec!["branch", "--sort=version:refname", "-r"],
            vec!["branch", "--sort=-version:refname", "-r"],
            vec!["branch", "--sort=objectname", "-r"],
            vec!["branch", "--sort=-objectname", "-r"],
            vec!["branch", "--sort=objecttype", "-r"],
            vec!["branch", "--sort=-objecttype", "-r"],
            vec!["branch", "--sort=objectsize", "-r"],
            vec!["branch", "--sort=-objectsize", "-r"],
            vec!["branch", "--sort=upstream", "-r"],
            vec!["branch", "--sort=-upstream", "-r"],
            vec!["branch", "--sort=push", "-r"],
            vec!["branch", "--sort=-push", "-r"],
            vec!["branch", "--sort", "refname", "-r"],
            vec!["branch", "--sort", "-refname", "-r"],
            vec!["branch", "--sort", "v:refname", "-r"],
            vec!["branch", "--sort", "-v:refname", "-r"],
            vec!["branch", "--sort", "objectname", "-r"],
            vec!["branch", "--sort", "-objectname", "-r"],
            vec!["branch", "--sort", "objecttype", "-r"],
            vec!["branch", "--sort", "-objecttype", "-r"],
            vec!["branch", "--sort", "objectsize", "-r"],
            vec!["branch", "--sort", "-objectsize", "-r"],
            vec!["branch", "--sort", "upstream", "-r"],
            vec!["branch", "--sort", "-upstream", "-r"],
            vec!["branch", "--sort", "push", "-r"],
            vec!["branch", "--sort", "-push", "-r"],
            vec!["branch", "--no-sort", "-r"],
            vec!["branch", "-a", "--sort=refname"],
            vec!["branch", "-a", "--sort=-refname"],
            vec!["branch", "-a", "--sort=version:refname"],
            vec!["branch", "-a", "--sort=-version:refname"],
            vec!["branch", "-a", "--sort=objectname"],
            vec!["branch", "-a", "--sort=-objectname"],
            vec!["branch", "-a", "--sort=objecttype"],
            vec!["branch", "-a", "--sort=-objecttype"],
            vec!["branch", "-a", "--sort=objectsize"],
            vec!["branch", "-a", "--sort=-objectsize"],
            vec!["branch", "-a", "--sort=committerdate"],
            vec!["branch", "-a", "--sort=-committerdate"],
            vec!["branch", "-a", "--sort", "authordate"],
            vec!["branch", "-a", "--sort", "-creatordate"],
            vec!["branch", "-a", "--sort=upstream"],
            vec!["branch", "-a", "--sort=-upstream"],
            vec!["branch", "-a", "--sort=push"],
            vec!["branch", "-a", "--sort=-push"],
            vec!["branch", "-a", "--sort", "refname"],
            vec!["branch", "-a", "--sort", "-refname"],
            vec!["branch", "-a", "--sort", "v:refname"],
            vec!["branch", "-a", "--sort", "-v:refname"],
            vec!["branch", "-a", "--sort", "objectname"],
            vec!["branch", "-a", "--sort", "-objectname"],
            vec!["branch", "-a", "--sort", "objecttype"],
            vec!["branch", "-a", "--sort", "-objecttype"],
            vec!["branch", "-a", "--sort", "objectsize"],
            vec!["branch", "-a", "--sort", "-objectsize"],
            vec!["branch", "-a", "--sort", "upstream"],
            vec!["branch", "-a", "--sort", "-upstream"],
            vec!["branch", "-a", "--sort", "push"],
            vec!["branch", "-a", "--sort", "-push"],
            vec!["branch", "-a", "--no-sort"],
            vec!["branch", "-a", "--sort=refname", "--no-sort"],
            vec!["branch", "-a", "--sort=-refname", "--no-sort"],
            vec!["branch", "-a", "--sort=version:refname", "--no-sort"],
            vec!["branch", "-a", "--sort=-version:refname", "--no-sort"],
            vec!["branch", "-a", "--sort=objectname", "--no-sort"],
            vec!["branch", "-a", "--sort=-objectname", "--no-sort"],
            vec!["branch", "-a", "--sort=objecttype", "--no-sort"],
            vec!["branch", "-a", "--sort=-objecttype", "--no-sort"],
            vec!["branch", "-a", "--sort=objectsize", "--no-sort"],
            vec!["branch", "-a", "--sort=-objectsize", "--no-sort"],
            vec!["branch", "-a", "--sort=committerdate", "--no-sort"],
            vec!["branch", "-a", "--sort=-committerdate", "--no-sort"],
            vec!["branch", "-a", "--sort=upstream", "--no-sort"],
            vec!["branch", "-a", "--sort=-upstream", "--no-sort"],
            vec!["branch", "-a", "--sort=push", "--no-sort"],
            vec!["branch", "-a", "--sort=-push", "--no-sort"],
            vec!["branch", "-a", "--no-sort", "--sort=refname"],
            vec!["branch", "-a", "--no-sort", "--sort=-refname"],
            vec!["branch", "-a", "--no-sort", "--sort=version:refname"],
            vec!["branch", "-a", "--no-sort", "--sort=-version:refname"],
            vec!["branch", "-a", "--no-sort", "--sort=objectname"],
            vec!["branch", "-a", "--no-sort", "--sort=-objectname"],
            vec!["branch", "-a", "--no-sort", "--sort=objecttype"],
            vec!["branch", "-a", "--no-sort", "--sort=-objecttype"],
            vec!["branch", "-a", "--no-sort", "--sort=objectsize"],
            vec!["branch", "-a", "--no-sort", "--sort=-objectsize"],
            vec!["branch", "-a", "--no-sort", "--sort=committerdate"],
            vec!["branch", "-a", "--no-sort", "--sort=-committerdate"],
            vec!["branch", "-a", "--no-sort", "--sort=upstream"],
            vec!["branch", "-a", "--no-sort", "--sort=-upstream"],
            vec!["branch", "-a", "--no-sort", "--sort=push"],
            vec!["branch", "-a", "--no-sort", "--sort=-push"],
            vec!["branch", "-a", "--sort", "refname", "--no-sort"],
            vec!["branch", "-a", "--sort", "-refname", "--no-sort"],
            vec!["branch", "-a", "--sort", "version:refname", "--no-sort"],
            vec!["branch", "-a", "--sort", "-version:refname", "--no-sort"],
            vec!["branch", "-a", "--sort", "objectname", "--no-sort"],
            vec!["branch", "-a", "--sort", "-objectname", "--no-sort"],
            vec!["branch", "-a", "--sort", "objecttype", "--no-sort"],
            vec!["branch", "-a", "--sort", "-objecttype", "--no-sort"],
            vec!["branch", "-a", "--sort", "objectsize", "--no-sort"],
            vec!["branch", "-a", "--sort", "-objectsize", "--no-sort"],
            vec!["branch", "-a", "--sort", "authordate", "--no-sort"],
            vec!["branch", "-a", "--sort", "-creatordate", "--no-sort"],
            vec!["branch", "-a", "--sort", "upstream", "--no-sort"],
            vec!["branch", "-a", "--sort", "-upstream", "--no-sort"],
            vec!["branch", "-a", "--sort", "push", "--no-sort"],
            vec!["branch", "-a", "--sort", "-push", "--no-sort"],
            vec!["branch", "-a", "--no-sort", "--sort", "refname"],
            vec!["branch", "-a", "--no-sort", "--sort", "-refname"],
            vec!["branch", "-a", "--no-sort", "--sort", "version:refname"],
            vec!["branch", "-a", "--no-sort", "--sort", "-version:refname"],
            vec!["branch", "-a", "--no-sort", "--sort", "objectname"],
            vec!["branch", "-a", "--no-sort", "--sort", "-objectname"],
            vec!["branch", "-a", "--no-sort", "--sort", "objecttype"],
            vec!["branch", "-a", "--no-sort", "--sort", "-objecttype"],
            vec!["branch", "-a", "--no-sort", "--sort", "objectsize"],
            vec!["branch", "-a", "--no-sort", "--sort", "-objectsize"],
            vec!["branch", "-a", "--no-sort", "--sort", "authordate"],
            vec!["branch", "-a", "--no-sort", "--sort", "-creatordate"],
            vec!["branch", "-a", "--no-sort", "--sort", "upstream"],
            vec!["branch", "-a", "--no-sort", "--sort", "-upstream"],
            vec!["branch", "-a", "--no-sort", "--sort", "push"],
            vec!["branch", "-a", "--no-sort", "--sort", "-push"],
            vec!["branch", "--sort=refname", "-a"],
            vec!["branch", "--sort=-refname", "-a"],
            vec!["branch", "--sort=version:refname", "-a"],
            vec!["branch", "--sort=-version:refname", "-a"],
            vec!["branch", "--sort=objectname", "-a"],
            vec!["branch", "--sort=-objectname", "-a"],
            vec!["branch", "--sort=objecttype", "-a"],
            vec!["branch", "--sort=-objecttype", "-a"],
            vec!["branch", "--sort=objectsize", "-a"],
            vec!["branch", "--sort=-objectsize", "-a"],
            vec!["branch", "--sort=upstream", "-a"],
            vec!["branch", "--sort=-upstream", "-a"],
            vec!["branch", "--sort=push", "-a"],
            vec!["branch", "--sort=-push", "-a"],
            vec!["branch", "--sort", "refname", "-a"],
            vec!["branch", "--sort", "-refname", "-a"],
            vec!["branch", "--sort", "v:refname", "-a"],
            vec!["branch", "--sort", "-v:refname", "-a"],
            vec!["branch", "--sort", "objectname", "-a"],
            vec!["branch", "--sort", "-objectname", "-a"],
            vec!["branch", "--sort", "objecttype", "-a"],
            vec!["branch", "--sort", "-objecttype", "-a"],
            vec!["branch", "--sort", "objectsize", "-a"],
            vec!["branch", "--sort", "-objectsize", "-a"],
            vec!["branch", "--sort", "upstream", "-a"],
            vec!["branch", "--sort", "-upstream", "-a"],
            vec!["branch", "--sort", "push", "-a"],
            vec!["branch", "--sort", "-push", "-a"],
            vec!["branch", "--no-sort", "-a"],
            vec!["branch", "--no-delete"],
            vec!["branch", "--no-list"],
            vec!["branch", "--no-show-current"],
            vec!["branch", "--show-current", "feature/foo"],
            vec!["branch", "feature/foo", "--show-current"],
            vec!["branch", "--show-current", "--", "feature/foo"],
            vec!["branch", "--no-show-current", "--show-current"],
            vec!["branch", "--show-current", "--no-show-current"],
            vec!["branch", "--show-current", "--no-show-current", "--"],
            vec!["branch", "--list", "--no-delete"],
            vec!["branch", "--list", "--no-list"],
            vec!["branch", "--list", "--no-show-current"],
            vec!["branch", "--no-delete", "--list", "feature/*"],
            vec!["branch", "--no-list", "--list", "Feature/*"],
            vec!["branch", "--no-show-current", "--list", "feature/*"],
            vec!["branch", "-r", "--no-delete"],
            vec!["branch", "-r", "--no-list"],
            vec!["branch", "-r", "--no-show-current"],
            vec!["branch", "-r", "--list", "--no-delete", "origin/*"],
            vec!["branch", "-r", "--no-list", "--list", "Origin/*"],
            vec!["branch", "-r", "--no-show-current", "--list", "origin/*"],
            vec!["branch", "-a", "--no-delete"],
            vec!["branch", "-a", "--no-list"],
            vec!["branch", "-a", "--no-show-current"],
            vec!["branch", "-a", "--list", "--no-delete", "origin/*"],
            vec!["branch", "-a", "--no-list", "--list", "feature/*"],
            vec!["branch", "-a", "--no-show-current", "--list", "origin/*"],
            vec!["branch", "--omit-empty"],
            vec!["branch", "--no-omit-empty"],
            vec!["branch", "--omit-empty", "--no-omit-empty"],
            vec!["branch", "--no-omit-empty", "--omit-empty"],
            vec!["branch", "--list", "--omit-empty"],
            vec!["branch", "--list", "--no-omit-empty"],
            vec!["branch", "--omit-empty", "--list"],
            vec!["branch", "--no-omit-empty", "--list"],
            vec!["branch", "--list", "--omit-empty", "feature/*"],
            vec!["branch", "--no-omit-empty", "--list", "Feature/*"],
            vec![
                "branch",
                "--omit-empty",
                "--no-omit-empty",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--no-omit-empty",
                "--omit-empty",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--omit-empty",
                "--no-omit-empty",
                "feature/*",
            ],
            vec!["branch", "-r", "--omit-empty"],
            vec!["branch", "-r", "--no-omit-empty"],
            vec!["branch", "-r", "--omit-empty", "--no-omit-empty"],
            vec!["branch", "-r", "--no-omit-empty", "--omit-empty"],
            vec!["branch", "--omit-empty", "-r"],
            vec!["branch", "--no-omit-empty", "-r"],
            vec!["branch", "-r", "--list", "--omit-empty"],
            vec!["branch", "-r", "--list", "--no-omit-empty"],
            vec!["branch", "-r", "--omit-empty", "--list", "origin/*"],
            vec!["branch", "-r", "--no-omit-empty", "--list", "Origin/*"],
            vec![
                "branch",
                "-r",
                "--omit-empty",
                "--no-omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-omit-empty",
                "--omit-empty",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--omit-empty",
                "--no-omit-empty",
                "origin/*",
            ],
            vec!["branch", "-a", "--omit-empty"],
            vec!["branch", "-a", "--no-omit-empty"],
            vec!["branch", "-a", "--omit-empty", "--no-omit-empty"],
            vec!["branch", "-a", "--no-omit-empty", "--omit-empty"],
            vec!["branch", "--omit-empty", "-a"],
            vec!["branch", "--no-omit-empty", "-a"],
            vec!["branch", "-a", "--list", "--omit-empty"],
            vec!["branch", "-a", "--list", "--no-omit-empty"],
            vec!["branch", "-a", "--omit-empty", "--list", "origin/*"],
            vec!["branch", "-a", "--no-omit-empty", "--list", "feature/*"],
            vec![
                "branch",
                "-a",
                "--omit-empty",
                "--no-omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-omit-empty",
                "--omit-empty",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--omit-empty",
                "--no-omit-empty",
                "origin/*",
            ],
            vec!["branch", "--ignore-case"],
            vec!["branch", "-i"],
            vec!["branch", "--no-ignore-case"],
            vec!["branch", "--list", "--ignore-case"],
            vec!["branch", "--ignore-case", "--list"],
            vec!["branch", "--list", "--no-ignore-case"],
            vec!["branch", "--no-ignore-case", "--list"],
            vec!["branch", "-r", "--ignore-case"],
            vec!["branch", "-r", "-i"],
            vec!["branch", "-r", "--no-ignore-case"],
            vec!["branch", "--ignore-case", "-r"],
            vec!["branch", "-i", "-r"],
            vec!["branch", "--no-ignore-case", "-r"],
            vec!["branch", "-a", "--ignore-case"],
            vec!["branch", "-a", "-i"],
            vec!["branch", "-a", "--no-ignore-case"],
            vec!["branch", "--ignore-case", "-a"],
            vec!["branch", "-i", "-a"],
            vec!["branch", "--no-ignore-case", "-a"],
            vec!["branch", "--ignore-case", "--list", "FEATURE/*"],
            vec!["branch", "-i", "--list", "FEATURE/*"],
            vec!["branch", "--list", "--ignore-case", "FEATURE/*"],
            vec![
                "branch",
                "--list",
                "--ignore-case",
                "--no-ignore-case",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--ignore-case",
                "--list",
                "--no-ignore-case",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "FEATURE/*",
            ],
            vec!["branch", "-r", "--ignore-case", "--list", "ORIGIN/*"],
            vec!["branch", "-r", "--list", "--ignore-case", "ORIGIN/*"],
            vec![
                "branch",
                "-r",
                "--list",
                "--ignore-case",
                "--no-ignore-case",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--ignore-case",
                "--list",
                "--no-ignore-case",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec!["branch", "-a", "--ignore-case", "--list", "ORIGIN/*"],
            vec!["branch", "-a", "--list", "--ignore-case", "ORIGIN/*"],
            vec![
                "branch",
                "-a",
                "--list",
                "--ignore-case",
                "--no-ignore-case",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--ignore-case",
                "--list",
                "--no-ignore-case",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec!["branch", "--format=%(refname:short)"],
            vec!["branch", "--format", "%(refname)|%(objectname:short)"],
            vec!["branch", "--format=%(HEAD) %(refname:short)"],
            vec![
                "branch",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
            ],
            vec!["branch", "--format", "%(refname:short)|%(objectsize:disk)"],
            vec![
                "branch",
                "--format=%(refname:short)|%(upstream:short)|%(upstream:trackshort)",
            ],
            vec![
                "branch",
                "--format",
                "%(refname:short)|%(push:short)|%(push:trackshort)",
            ],
            vec!["branch", "--no-format"],
            vec!["branch", "--format=%(refname:short)", "--no-format"],
            vec!["branch", "--no-format", "--format=%(refname:short)"],
            vec!["branch", "--list", "--no-format", "feature/*"],
            vec!["branch", "--no-format", "--list", "Feature/*"],
            vec![
                "branch",
                "--format=%(refname:short)",
                "--no-format",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--no-format",
                "--format=%(refname:short)",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format=%(refname:short)",
                "--no-format",
                "feature/*",
            ],
            vec![
                "branch",
                "--format",
                "%(refname:short)",
                "--no-format",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--no-format",
                "--format",
                "%(refname:short)",
                "--list",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "%(refname:short)",
                "--no-format",
                "feature/*",
            ],
            vec!["branch", "--format=", "--omit-empty"],
            vec!["branch", "--omit-empty", "--format="],
            vec!["branch", "--format=", "--no-omit-empty"],
            vec!["branch", "--format", "", "--omit-empty"],
            vec!["branch", "--format", "", "--no-omit-empty"],
            vec![
                "branch",
                "--format",
                "",
                "--omit-empty",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--format",
                "",
                "--no-omit-empty",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "",
                "--omit-empty",
                "Feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "",
                "--no-omit-empty",
                "Feature/*",
            ],
            vec!["branch", "--format=", "--omit-empty", "--list", "feature/*"],
            vec![
                "branch",
                "--format=",
                "--no-omit-empty",
                "--list",
                "feature/*",
            ],
            vec!["branch", "--list", "--format=", "--omit-empty", "Feature/*"],
            vec![
                "branch",
                "--list",
                "--format=",
                "--no-omit-empty",
                "Feature/*",
            ],
            vec!["branch", "--format=%(refname:short)", "--list", "feature/*"],
            vec!["branch", "--list", "--format=%(refname:short)", "Feature/*"],
            vec![
                "branch",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "%(refname:short)|%(objectsize:disk)",
                "Feature/*",
            ],
            vec![
                "branch",
                "--format=%(refname:short)|%(upstream:remotename)|%(upstream:remoteref)",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "%(refname:short)|%(push:remotename)|%(push:trackshort)",
                "Feature/*",
            ],
            vec![
                "branch",
                "--format=%(refname:short)",
                "--ignore-case",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--ignore-case",
                "--format=%(refname:short)",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--list",
                "--ignore-case",
                "--format=%(refname:short)",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--format=%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--format",
                "%(refname:short)",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--ignore-case",
                "--format",
                "%(refname:short)",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--list",
                "--ignore-case",
                "--format",
                "%(refname:short)",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "FEATURE/*",
            ],
            vec![
                "branch",
                "--list",
                "--format",
                "%(refname:short)",
                "Feature/*",
            ],
            vec!["branch", "-r", "--format=%(refname:short)"],
            vec!["branch", "-r", "--format", "%(refname:short)"],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)|%(objectsize:disk)",
            ],
            vec!["branch", "-r", "--no-format"],
            vec!["branch", "--no-format", "-r"],
            vec!["branch", "-r", "--list", "--no-format", "origin/*"],
            vec!["branch", "-r", "--no-format", "--list", "Origin/*"],
            vec!["branch", "-r", "--format=%(refname:short)", "--no-format"],
            vec!["branch", "-r", "--no-format", "--format=%(refname:short)"],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)",
                "--no-format",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-format",
                "--format=%(refname:short)",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format=%(refname:short)",
                "--no-format",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)",
                "--no-format",
            ],
            vec![
                "branch",
                "-r",
                "--no-format",
                "--format",
                "%(refname:short)",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)",
                "--no-format",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--no-format",
                "--format",
                "%(refname:short)",
                "--list",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "%(refname:short)",
                "--no-format",
                "origin/*",
            ],
            vec!["branch", "-r", "--format=", "--omit-empty"],
            vec!["branch", "-r", "--format=", "--no-omit-empty"],
            vec!["branch", "-r", "--format", "", "--omit-empty"],
            vec!["branch", "-r", "--format", "", "--no-omit-empty"],
            vec![
                "branch",
                "-r",
                "--format",
                "",
                "--omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "",
                "--omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "",
                "--omit-empty",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "",
                "--no-omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--format=",
                "--omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format=",
                "--omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format=",
                "--omit-empty",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format=",
                "--no-omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "%(refname:short)|%(objectsize:disk)",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--ignore-case",
                "--format",
                "%(refname:short)",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)",
                "--ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--ignore-case",
                "--format=%(refname:short)",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--format=%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-r",
                "--format",
                "%(refname:short)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format",
                "%(refname:short)",
                "Origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                "--format=%(refname:short)",
                "Origin/*",
            ],
            vec!["branch", "-a", "--format=%(refname:short)"],
            vec!["branch", "-a", "--format", "%(refname:short)"],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)|%(objectsize:disk)",
            ],
            vec!["branch", "-a", "--no-format"],
            vec!["branch", "--no-format", "-a"],
            vec!["branch", "-a", "--list", "--no-format", "origin/*"],
            vec!["branch", "-a", "--no-format", "--list", "feature/*"],
            vec!["branch", "-a", "--format=%(refname:short)", "--no-format"],
            vec!["branch", "-a", "--no-format", "--format=%(refname:short)"],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)",
                "--no-format",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-format",
                "--format=%(refname:short)",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format=%(refname:short)",
                "--no-format",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)",
                "--no-format",
            ],
            vec![
                "branch",
                "-a",
                "--no-format",
                "--format",
                "%(refname:short)",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)",
                "--no-format",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--no-format",
                "--format",
                "%(refname:short)",
                "--list",
                "feature/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "%(refname:short)",
                "--no-format",
                "origin/*",
            ],
            vec!["branch", "-a", "--format=", "--omit-empty"],
            vec!["branch", "-a", "--format=", "--no-omit-empty"],
            vec!["branch", "-a", "--format", "", "--omit-empty"],
            vec!["branch", "-a", "--format", "", "--no-omit-empty"],
            vec![
                "branch",
                "-a",
                "--format",
                "",
                "--omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "",
                "--omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "",
                "--omit-empty",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "",
                "--no-omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--format=",
                "--omit-empty",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format=",
                "--omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format=",
                "--omit-empty",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format=",
                "--no-omit-empty",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)|%(objecttype)|%(objectsize)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "%(refname:short)|%(objectsize:disk)",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--ignore-case",
                "--format",
                "%(refname:short)",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)",
                "--ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--ignore-case",
                "--format=%(refname:short)",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--format=%(refname:short)",
                "--ignore-case",
                "--no-ignore-case",
                "--list",
                "ORIGIN/*",
            ],
            vec![
                "branch",
                "-a",
                "--format",
                "%(refname:short)",
                "--list",
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format",
                "%(refname:short)",
                "Origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                "--format=%(refname:short)",
                "Origin/*",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
        for args in [
            vec!["branch", "--show-current=feature/foo"],
            vec!["branch", "--no-show-current=feature/foo"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
        let contains_base_eq = format!("--contains={base_oid}");
        let contains_eq = format!("--contains={main_oid}");
        let expected = git(&root, &["branch", contains_eq.as_str()]);
        let actual = git_rs(&root, &["branch", contains_eq.as_str()]);
        assert_eq!(actual, expected, "git-rs output differed for --contains=");
        let points_at_eq = format!("--points-at={main_oid}");
        let expected = git(&root, &["branch", points_at_eq.as_str()]);
        let actual = git_rs(&root, &["branch", points_at_eq.as_str()]);
        assert_eq!(actual, expected, "git-rs output differed for --points-at=");
        let expected = git(&root, &["branch", "--list", points_at_eq.as_str(), "m*"]);
        let actual = git_rs(&root, &["branch", "--list", points_at_eq.as_str(), "m*"]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for --list --points-at= pattern"
        );
        let expected = git(&root, &["branch", points_at_eq.as_str(), "--no-points-at"]);
        let actual = git_rs(&root, &["branch", points_at_eq.as_str(), "--no-points-at"]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for --points-at= --no-points-at"
        );
        let expected = git(&root, &["branch", "--no-points-at", points_at_eq.as_str()]);
        let actual = git_rs(&root, &["branch", "--no-points-at", points_at_eq.as_str()]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for --no-points-at --points-at="
        );
        let expected = git(&root, &["branch", "-r", points_at_eq.as_str()]);
        let actual = git_rs(&root, &["branch", "-r", points_at_eq.as_str()]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for -r --points-at="
        );
        let expected = git(
            &root,
            &["branch", "-r", "--list", points_at_eq.as_str(), "origin/*"],
        );
        let actual = git_rs(
            &root,
            &["branch", "-r", "--list", points_at_eq.as_str(), "origin/*"],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -r --list --points-at= pattern"
        );
        let expected = git(
            &root,
            &["branch", "-r", points_at_eq.as_str(), "--no-points-at"],
        );
        let actual = git_rs(
            &root,
            &["branch", "-r", points_at_eq.as_str(), "--no-points-at"],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -r --points-at= --no-points-at"
        );
        let expected = git(
            &root,
            &["branch", "-r", "--no-points-at", points_at_eq.as_str()],
        );
        let actual = git_rs(
            &root,
            &["branch", "-r", "--no-points-at", points_at_eq.as_str()],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -r --no-points-at --points-at="
        );
        let expected = git(&root, &["branch", "-a", points_at_eq.as_str()]);
        let actual = git_rs(&root, &["branch", "-a", points_at_eq.as_str()]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for -a --points-at="
        );
        let expected = git(
            &root,
            &["branch", "-a", "--list", points_at_eq.as_str(), "origin/*"],
        );
        let actual = git_rs(
            &root,
            &["branch", "-a", "--list", points_at_eq.as_str(), "origin/*"],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -a --list --points-at= pattern"
        );
        let expected = git(
            &root,
            &["branch", "-a", points_at_eq.as_str(), "--no-points-at"],
        );
        let actual = git_rs(
            &root,
            &["branch", "-a", points_at_eq.as_str(), "--no-points-at"],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -a --points-at= --no-points-at"
        );
        let expected = git(
            &root,
            &["branch", "-a", "--no-points-at", points_at_eq.as_str()],
        );
        let actual = git_rs(
            &root,
            &["branch", "-a", "--no-points-at", points_at_eq.as_str()],
        );
        assert_eq!(
            actual, expected,
            "git-rs output differed for -a --no-points-at --points-at="
        );
        let no_contains_eq = format!("--no-contains={main_oid}");
        let expected = git(&root, &["branch", no_contains_eq.as_str()]);
        let actual = git_rs(&root, &["branch", no_contains_eq.as_str()]);
        assert_eq!(
            actual, expected,
            "git-rs output differed for --no-contains="
        );
        let merged_eq = format!("--merged={main_oid}");
        let expected = git(&root, &["branch", merged_eq.as_str()]);
        let actual = git_rs(&root, &["branch", merged_eq.as_str()]);
        assert_eq!(actual, expected, "git-rs output differed for --merged=");
        let no_merged_base_eq = format!("--no-merged={base_oid}");
        let no_merged_eq = format!("--no-merged={main_oid}");
        for args in [
            vec!["branch", contains_base_eq.as_str(), no_contains_eq.as_str()],
            vec!["branch", no_contains_eq.as_str(), contains_base_eq.as_str()],
            vec!["branch", "--list", contains_base_eq.as_str(), "feature/*"],
            vec![
                "branch",
                "--list",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
            ],
            vec![
                "branch",
                "--list",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
                "feature/*",
            ],
            vec![
                "branch",
                "-r",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
            ],
            vec!["branch", "-r", "--list", contains_eq.as_str(), "origin/*"],
            vec![
                "branch",
                "-r",
                "--list",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
            ],
            vec![
                "branch",
                "-a",
                "--list",
                contains_base_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                contains_base_eq.as_str(),
                no_contains_eq.as_str(),
                "origin/*",
            ],
            vec!["branch", "--list", no_contains_eq.as_str(), "feature/*"],
            vec![
                "branch",
                "-r",
                "--list",
                no_contains_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                no_contains_eq.as_str(),
                "origin/*",
            ],
            vec!["branch", merged_eq.as_str(), no_merged_base_eq.as_str()],
            vec!["branch", no_merged_base_eq.as_str(), merged_eq.as_str()],
            vec!["branch", "--list", merged_eq.as_str(), "m*"],
            vec![
                "branch",
                "--list",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
            ],
            vec![
                "branch",
                "--list",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
                "m*",
            ],
            vec!["branch", "--list", no_merged_base_eq.as_str(), "feature/*"],
            vec![
                "branch",
                "-r",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
            ],
            vec!["branch", "-r", "--list", merged_eq.as_str(), "origin/*"],
            vec![
                "branch",
                "-r",
                "--list",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-r",
                "--list",
                no_merged_base_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
            ],
            vec!["branch", "-a", "--list", merged_eq.as_str(), "origin/*"],
            vec![
                "branch",
                "-a",
                "--list",
                merged_eq.as_str(),
                no_merged_base_eq.as_str(),
                "origin/*",
            ],
            vec![
                "branch",
                "-a",
                "--list",
                no_merged_base_eq.as_str(),
                "origin/*",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
        let expected = git(&root, &["branch", no_merged_eq.as_str()]);
        let actual = git_rs(&root, &["branch", no_merged_eq.as_str()]);
        assert_eq!(actual, expected, "git-rs output differed for --no-merged=");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
