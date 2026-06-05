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
    let output = Command::new("git")
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
    run("git", cwd, args)
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
    git(root, &["init", "-q"]);
    prepare_repo_contents(root);
}

fn prepare_sha256_repo(root: &Path) {
    git(root, &["init", "-q", "--object-format=sha256"]);
    prepare_repo_contents(root);
}

fn prepare_repo_contents(root: &Path) {
    fs::create_dir_all(root.join("dir")).expect("create dir");
    fs::write(root.join("file.txt"), b"base\n").expect("write file");
    fs::write(root.join("dir/nested.txt"), b"nested\n").expect("write nested");
    git(root, &["add", "file.txt", "dir/nested.txt"]);
    run_with_identity(root, &["commit", "-m", "base", "-q"]);
}

#[test]
fn rm_tracked_paths_match_upstream_git() {
    let root = unique_temp_dir("rm-tracked-paths");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "file.txt", "dir/nested.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !rust.join("file.txt").exists(),
            "sley rm left tracked file"
        );
        assert!(
            !rust.join("dir/nested.txt").exists(),
            "sley rm left nested tracked file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_sha256_tracked_paths_match_upstream_git() {
    let root = unique_temp_dir("rm-sha256");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_sha256_repo(&upstream);
        prepare_sha256_repo(&rust);

        let args = ["rm", "file.txt", "dir/nested.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            upstream.join("file.txt").exists(),
            rust.join("file.txt").exists(),
            "worktree file presence differed"
        );
        assert_eq!(
            upstream.join("dir/nested.txt").exists(),
            rust.join("dir/nested.txt").exists(),
            "nested worktree file presence differed"
        );
        for args in [
            vec!["diff", "--cached", "--name-status"],
            vec!["status", "--short"],
        ] {
            let expected = run_output("git", &upstream, &args);
            let actual = run_output("git", &rust, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_cached_keeps_worktree_and_removes_index_like_upstream_git() {
    let root = unique_temp_dir("rm-cached");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write modified file");
        }

        let args = ["rm", "--cached", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            b"worktree\n",
            "sley rm --cached changed worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm --cached"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --cached"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_dry_run_reports_paths_without_removing_like_upstream_git() {
    let root = unique_temp_dir("rm-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "-n", "file.txt", "dir/nested.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(rust.join("file.txt").exists(), "sley rm -n removed file");
        assert!(
            rust.join("dir/nested.txt").exists(),
            "sley rm -n removed nested file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm -n"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm -n"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_dry_run_overrides_dry_run_like_upstream_git() {
    let root = unique_temp_dir("rm-no-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "-n", "--no-dry-run", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(!rust.join("file.txt").exists(), "sley rm left file");
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --no-dry-run"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_quiet_overrides_quiet_like_upstream_git() {
    let root = unique_temp_dir("rm-no-quiet");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "-q", "--no-quiet", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --no-quiet"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_cached_overrides_cached_like_upstream_git() {
    let root = unique_temp_dir("rm-no-cached");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "--cached", "--no-cached", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !rust.join("file.txt").exists(),
            "sley rm --no-cached left worktree file"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --no-cached"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_force_overrides_force_like_upstream_git() {
    let root = unique_temp_dir("rm-no-force");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"modified\n").expect("modify tracked file");
        }

        let args = ["rm", "-f", "--no-force", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            rust.join("file.txt").exists(),
            "sley rm --no-force removed modified file"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_combined_short_options_match_upstream_git() {
    let root = unique_temp_dir("rm-combined-short-options");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "-rn", "dir"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            rust.join("dir/nested.txt").exists(),
            "sley rm -rn removed nested file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm -rn"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm -rn"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_ignore_unmatch_skips_missing_paths_like_upstream_git() {
    let root = unique_temp_dir("rm-ignore-unmatch");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "--ignore-unmatch", "missing.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            rust.join("file.txt").exists(),
            "sley rm --ignore-unmatch removed tracked file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm --ignore-unmatch"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --ignore-unmatch"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_ignore_unmatch_removes_matched_paths_like_upstream_git() {
    let root = unique_temp_dir("rm-ignore-unmatch-mixed");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "--ignore-unmatch", "missing.txt", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !rust.join("file.txt").exists(),
            "sley rm --ignore-unmatch left matched tracked file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm --ignore-unmatch with matched path"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --ignore-unmatch with matched path"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_ignore_unmatch_restores_missing_path_error_like_upstream_git() {
    let root = unique_temp_dir("rm-no-ignore-unmatch");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = [
            "rm",
            "--ignore-unmatch",
            "--no-ignore-unmatch",
            "missing.txt",
        ];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_sparse_flags_are_accepted_like_upstream_git() {
    let root = unique_temp_dir("rm-sparse-flags");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "--sparse", "--no-sparse", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm sparse flags"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_pathspec_from_file_matches_upstream_git() {
    let root = unique_temp_dir("rm-pathspec-from-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\ndir/nested.txt\n")
                .expect("write pathspec file");
        }

        let args = ["rm", "--pathspec-from-file=pathspecs"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !rust.join("file.txt").exists(),
            "sley rm --pathspec-from-file left tracked file"
        );
        assert!(
            !rust.join("dir/nested.txt").exists(),
            "sley rm --pathspec-from-file left nested file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm --pathspec-from-file"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --pathspec-from-file"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_pathspec_file_nul_matches_upstream_git() {
    let root = unique_temp_dir("rm-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\0dir/nested.txt\0")
                .expect("write pathspec file");
        }

        let args = [
            "rm",
            "--pathspec-file-nul",
            "--pathspec-from-file",
            "pathspecs",
        ];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(
            !rust.join("file.txt").exists(),
            "sley rm --pathspec-file-nul left tracked file"
        );
        assert!(
            !rust.join("dir/nested.txt").exists(),
            "sley rm --pathspec-file-nul left nested file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm --pathspec-file-nul"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --pathspec-file-nul"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_pathspec_file_nul_overrides_previous_value_like_upstream_git() {
    let root = unique_temp_dir("rm-no-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\ndir/nested.txt\n")
                .expect("write pathspec file");
        }

        let args = [
            "rm",
            "--pathspec-file-nul",
            "--no-pathspec-file-nul",
            "--pathspec-from-file=pathspecs",
        ];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm --no-pathspec-file-nul"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_no_pathspec_from_file_keeps_inline_rejection_like_upstream_git() {
    let root = unique_temp_dir("rm-no-pathspec-from-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\n").expect("write pathspec file");
        }

        let args = [
            "rm",
            "--pathspec-from-file=pathspecs",
            "--no-pathspec-from-file",
            "dir/nested.txt",
        ];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_pathspec_from_file_rejects_inline_pathspecs_like_upstream_git() {
    let root = unique_temp_dir("rm-pathspec-from-file-mixed");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\n").expect("write pathspec file");
        }

        let args = ["rm", "--pathspec-from-file=pathspecs", "dir/nested.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_pathspec_file_nul_requires_pathspec_from_file_like_upstream_git() {
    let root = unique_temp_dir("rm-pathspec-file-nul-without-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["rm", "--pathspec-file-nul", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rm_force_removes_modified_paths_like_upstream_git() {
    let root = unique_temp_dir("rm-force");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write modified file");
        }

        let args = ["rm", "-f", "file.txt"];
        let expected = run_output("git", &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(!rust.join("file.txt").exists(), "sley rm -f left file");
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after rm -f"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after rm -f"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
