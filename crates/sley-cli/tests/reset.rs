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

fn prepare_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    prepare_repo_contents(root);
}

fn prepare_sha256_repo(root: &Path) {
    git(root, &["init", "-q", "--object-format=sha256", "-b", "main"]);
    prepare_repo_contents(root);
}

fn prepare_repo_contents(root: &Path) {
    fs::write(root.join("file.txt"), b"base\n").expect("write file");
    git(root, &["add", "file.txt"]);
    run_with_identity(root, &["commit", "-m", "base", "-q"]);
}

#[test]
fn reset_path_unstages_modified_file_like_upstream_git() {
    let root = unique_temp_dir("reset-modified");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            git(repo, &["add", "file.txt"]);
        }

        let args = ["reset", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_sha256_mixed_and_hard_match_upstream_git() {
    let root = unique_temp_dir("reset-sha256");
    let upstream_mixed = root.join("upstream-mixed");
    let rust_mixed = root.join("rust-mixed");
    let upstream_hard = root.join("upstream-hard");
    let rust_hard = root.join("rust-hard");
    for repo in [&upstream_mixed, &rust_mixed, &upstream_hard, &rust_hard] {
        fs::create_dir_all(repo).expect("create repo");
        prepare_sha256_repo(repo);
        fs::write(repo.join("file.txt"), b"second\n").expect("write second");
        git(repo, &["add", "file.txt"]);
        run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        fs::write(repo.join("file.txt"), b"dirty\n").expect("write dirty");
        git(repo, &["add", "file.txt"]);
    }
    {
        let args = ["reset", "--mixed", "HEAD~1"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream_mixed, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust_mixed, &args);
        assert_same_output(actual, expected, &args);

        let args = ["reset", "--hard", "HEAD~1"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream_hard, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust_hard, &args);
        assert_same_output(actual, expected, &args);

        for (upstream, rust) in [(&upstream_mixed, &rust_mixed), (&upstream_hard, &rust_hard)] {
            for args in [
                vec!["rev-parse", "HEAD"],
                vec!["status", "--short"],
                vec!["ls-files", "--stage"],
            ] {
                let expected = run_output(sley_testkit::oracle_git(), upstream, &args);
                let actual = run_output(sley_testkit::oracle_git(), rust, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_path_unstages_added_file_like_upstream_git() {
    let root = unique_temp_dir("reset-added");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "new.txt"]);
        }

        let args = ["reset", "new.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset added"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset added"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_quiet_suppresses_unstaged_summary_like_upstream_git() {
    let root = unique_temp_dir("reset-quiet");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            git(repo, &["add", "file.txt"]);
        }

        let args = ["reset", "-q", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_no_quiet_restores_unstaged_summary_like_upstream_git() {
    let root = unique_temp_dir("reset-no-quiet");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            git(repo, &["add", "file.txt"]);
        }

        let args = ["reset", "-q", "--no-quiet", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_pathspec_from_file_matches_upstream_git() {
    let root = unique_temp_dir("reset-pathspec-from-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "file.txt", "new.txt"]);
            fs::write(repo.join("pathspecs"), b"file.txt\nnew.txt\n").expect("write pathspec file");
        }

        let args = ["reset", "--pathspec-from-file=pathspecs"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --pathspec-from-file"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_pathspec_file_nul_matches_upstream_git() {
    let root = unique_temp_dir("reset-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "file.txt", "new.txt"]);
            fs::write(repo.join("pathspecs"), b"file.txt\0new.txt\0").expect("write pathspec file");
        }

        let args = [
            "reset",
            "--pathspec-file-nul",
            "--pathspec-from-file",
            "pathspecs",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --pathspec-file-nul"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_pathspec_file_option_errors_match_upstream_git() {
    let root = unique_temp_dir("reset-pathspec-file-errors");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\n").expect("write pathspec file");
        }

        for args in [
            ["reset", "--pathspec-file-nul", "file.txt"].as_slice(),
            ["reset", "--pathspec-from-file=pathspecs", "file.txt"].as_slice(),
            [
                "reset",
                "--pathspec-from-file=pathspecs",
                "--no-pathspec-from-file",
                "file.txt",
            ]
            .as_slice(),
            ["reset", "--hard", "--pathspec-from-file=pathspecs"].as_slice(),
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &upstream, args);
            let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, args);
            assert_same_output(actual, expected, args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_source_path_updates_index_without_moving_head_like_upstream_git() {
    let root = unique_temp_dir("reset-source-path");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "file.txt", "new.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            fs::write(repo.join("new.txt"), b"staged new\n").expect("write staged new file");
            git(repo, &["add", "file.txt", "new.txt"]);
        }

        let head_before = git(&upstream, &["rev-parse", "HEAD"]);
        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", target.as_str(), "--", "file.txt", "new.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["rev-parse", "HEAD"]),
            head_before,
            "reset <tree-ish> -- <path> moved HEAD"
        );
        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read file"),
            b"staged\n",
            "reset <tree-ish> -- <path> changed worktree file"
        );
        assert_eq!(
            fs::read(rust.join("new.txt")).expect("read new file"),
            b"staged new\n",
            "reset <tree-ish> -- <path> changed worktree new file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset <tree-ish> -- <path>"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset <tree-ish> -- <path>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_source_path_without_separator_matches_upstream_git() {
    let root = unique_temp_dir("reset-source-path-no-separator");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            git(repo, &["add", "file.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged file");
            git(repo, &["add", "file.txt"]);
        }

        let head_before = git(&upstream, &["rev-parse", "HEAD"]);
        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", target.as_str(), "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["rev-parse", "HEAD"]),
            head_before,
            "reset <tree-ish> <path> moved HEAD"
        );
        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read file"),
            b"staged\n",
            "reset <tree-ish> <path> changed worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset <tree-ish> <path>"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset <tree-ish> <path>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_hard_restores_index_and_worktree_like_upstream_git() {
    let root = unique_temp_dir("reset-hard");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"worktree\n").expect("modify file");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "new.txt"]);
        }

        let args = ["reset", "--hard"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read restored file"),
            b"base\n",
            "reset --hard did not restore tracked file"
        );
        assert!(
            !rust.join("new.txt").exists(),
            "reset --hard left staged added worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset --hard"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --hard"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_hard_quiet_suppresses_head_summary_like_upstream_git() {
    let root = unique_temp_dir("reset-hard-quiet");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"worktree\n").expect("modify file");
        }

        let args = ["reset", "--hard", "-q"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_hard_to_commit_moves_head_like_upstream_git() {
    let root = unique_temp_dir("reset-hard-commit");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            git(repo, &["add", "file.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", "--hard", target.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["rev-parse", "HEAD"]),
            git(&upstream, &["rev-parse", "HEAD"]),
            "HEAD differed after reset --hard <commit>"
        );
        assert_eq!(
            git(&rust, &["branch", "--show-current"]),
            git(&upstream, &["branch", "--show-current"]),
            "current branch differed after reset --hard <commit>"
        );
        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read reset file"),
            b"base\n",
            "reset --hard <commit> did not restore target tree"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --hard <commit>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_soft_to_commit_moves_head_only_like_upstream_git() {
    let root = unique_temp_dir("reset-soft-commit");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            git(repo, &["add", "file.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", "--soft", target.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["rev-parse", "HEAD"]),
            git(&upstream, &["rev-parse", "HEAD"]),
            "HEAD differed after reset --soft <commit>"
        );
        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read file"),
            b"second\n",
            "reset --soft changed worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset --soft <commit>"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --soft <commit>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_soft_rejects_paths_like_upstream_git() {
    let root = unique_temp_dir("reset-soft-path");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["reset", "--soft", "HEAD", "--", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_mixed_to_commit_moves_head_and_index_like_upstream_git() {
    let root = unique_temp_dir("reset-mixed-commit");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            git(repo, &["add", "file.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", "--mixed", target.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["rev-parse", "HEAD"]),
            git(&upstream, &["rev-parse", "HEAD"]),
            "HEAD differed after reset --mixed <commit>"
        );
        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read file"),
            b"second\n",
            "reset --mixed changed worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after reset --mixed <commit>"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after reset --mixed <commit>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reset_mixed_quiet_to_commit_suppresses_summary_like_upstream_git() {
    let root = unique_temp_dir("reset-mixed-quiet");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"second\n").expect("write second file");
            git(repo, &["add", "file.txt"]);
            run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = ["reset", "--mixed", "-q", target.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
