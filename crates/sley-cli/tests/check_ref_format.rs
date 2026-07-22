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

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), cwd, args);
    let actual = run_output(sley_testkit::sley_bin!(), cwd, args);
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
fn check_ref_format_matches_upstream_git() {
    let root = unique_temp_dir("check-ref-format");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        for args in [
            vec!["check-ref-format", "refs/heads/main"],
            vec!["check-ref-format", "main"],
            vec!["check-ref-format", "--allow-onelevel", "main"],
            vec!["check-ref-format", "refs/heads/.bad"],
            vec!["check-ref-format", "refs/heads/foo.lock/bar"],
            vec!["check-ref-format", "refs/heads/bad..name"],
            vec!["check-ref-format", "refs/heads/bad name"],
            vec!["check-ref-format", "refs/heads/@{bad"],
            vec!["check-ref-format", "--normalize", "/refs//heads/main"],
            vec!["check-ref-format", "--normalize", "main"],
            vec![
                "check-ref-format",
                "--normalize",
                "--allow-onelevel",
                "/main",
            ],
            vec!["check-ref-format", "--refspec-pattern", "refs/heads/*"],
            vec!["check-ref-format", "--refspec-pattern", "refs/heads/*/*"],
            vec!["check-ref-format", "--branch", "main"],
            vec!["check-ref-format", "--branch", "-bad"],
            vec!["check-ref-format"],
            vec!["check-ref-format", "refs/heads/main", "extra"],
            vec!["check-ref-format", "--unknown", "refs/heads/main"],
        ] {
            assert_status_stdout_stderr_match(&root, &args);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// t1402 #82/#84: `--branch @{-N}` expands via the HEAD reflog when a repository
/// is present, including when invoked from a subdirectory.
#[test]
fn check_ref_format_branch_prior_checkout_matches_upstream_git() {
    let root = unique_temp_dir("check-ref-format-prior");
    std::fs::create_dir_all(&root).expect("create temp dir");
    {
        let status = Command::new(sley_testkit::oracle_git())
            .current_dir(&root)
            .args(["init", "-q", "-b", "main"])
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");

        let tree = run_output(sley_testkit::oracle_git(), &root, &["write-tree"]);
        assert!(tree.status.success(), "write-tree failed");
        let tree_oid = String::from_utf8_lossy(&tree.stdout).trim().to_string();

        let sha1_out = Command::new(sley_testkit::oracle_git())
            .current_dir(&root)
            .args(["commit-tree", &tree_oid, "-m", "A"])
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.invalid")
            .env("GIT_AUTHOR_DATE", "@1000000000 +0000")
            .env("GIT_COMMITTER_NAME", "C")
            .env("GIT_COMMITTER_EMAIL", "c@example.invalid")
            .env("GIT_COMMITTER_DATE", "@1000000000 +0000")
            .output()
            .expect("commit-tree");
        assert!(sha1_out.status.success(), "commit-tree failed");
        let sha1 = String::from_utf8_lossy(&sha1_out.stdout).trim().to_string();

        for args in [
            ["update-ref", "refs/heads/main", &sha1],
            ["update-ref", "refs/remotes/origin/main", &sha1],
        ] {
            let st = Command::new(sley_testkit::oracle_git())
                .current_dir(&root)
                .args(args)
                .status()
                .expect("update-ref");
            assert!(st.success(), "update-ref failed for {args:?}");
        }
        for args in [["checkout", "main"], ["checkout", "origin/main"], ["checkout", "main"]] {
            let st = Command::new(sley_testkit::oracle_git())
                .current_dir(&root)
                .args(args)
                .status()
                .expect("checkout");
            assert!(st.success(), "checkout failed for {args:?}");
        }

        assert_status_stdout_stderr_match(&root, &["check-ref-format", "--branch", "@{-1}"]);
        assert_status_stdout_stderr_match(&root, &["check-ref-format", "--branch", "@{-2}"]);

        let subdir = root.join("subdir");
        std::fs::create_dir_all(&subdir).expect("subdir");
        assert_status_stdout_stderr_match(&subdir, &["check-ref-format", "--branch", "@{-1}"]);
    }
    let _ = std::fs::remove_dir_all(&root);
}
