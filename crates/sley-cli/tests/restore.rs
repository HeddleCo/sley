use std::fs;
use std::io::Write;
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

fn run_with_input(program: &str, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .take()
        .expect("interactive stdin")
        .write_all(input)
        .expect("write interactive input");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
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
    git(
        root,
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
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
fn restore_worktree_paths_from_index_match_upstream_git() {
    let root = unique_temp_dir("restore-worktree");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            git(repo, &["add", "file.txt"]);
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree content");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
        }

        let args = ["restore", "file.txt", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read restored file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "restored file content differed"
        );
        assert_eq!(
            fs::read(rust.join("dir/nested.txt")).expect("read restored nested file"),
            fs::read(upstream.join("dir/nested.txt")).expect("read upstream nested file"),
            "restored nested file content differed"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_sha256_worktree_and_staged_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-sha256");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_sha256_repo(&upstream);
        prepare_sha256_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"changed\n").expect("modify file");
        }
        let args = ["restore", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("modify file");
            git(repo, &["add", "file.txt"]);
        }
        let args = ["restore", "--staged", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        for args in [vec!["status", "--short"], vec!["ls-files", "--stage"]] {
            let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
            let actual = run_output(sley_testkit::oracle_git(), &rust, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_sha256_source_staged_and_worktree_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-sha256-source");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_sha256_repo(&upstream);
        prepare_sha256_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"changed\n").expect("modify file");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "file.txt", "new.txt"]);
        }

        let args = [
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "file.txt",
            "new.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            fs::read(rust.join("file.txt")).expect("read rust file"),
            "restored file content differed"
        );
        assert_eq!(
            upstream.join("new.txt").exists(),
            rust.join("new.txt").exists(),
            "restored new file presence differed"
        );
        for args in [vec!["status", "--short"], vec!["ls-files", "--stage"]] {
            let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
            let actual = run_output(sley_testkit::oracle_git(), &rust, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_pathspec_from_file_matches_upstream_git() {
    let root = unique_temp_dir("restore-pathspec-from-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            git(repo, &["add", "file.txt"]);
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree content");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
            fs::write(repo.join("pathspecs"), b"file.txt\ndir\n").expect("write pathspec file");
        }

        let args = ["restore", "--pathspec-from-file=pathspecs"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --pathspec-from-file"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_pathspec_file_nul_matches_upstream_git() {
    let root = unique_temp_dir("restore-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            git(repo, &["add", "file.txt"]);
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree content");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
            fs::write(repo.join("pathspecs"), b"file.txt\0dir\0").expect("write pathspec file");
        }

        let args = [
            "restore",
            "--pathspec-file-nul",
            "--pathspec-from-file",
            "pathspecs",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --pathspec-file-nul"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_pathspec_file_option_errors_match_upstream_git() {
    let root = unique_temp_dir("restore-pathspec-file-errors");
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
            ["restore", "--pathspec-file-nul", "file.txt"].as_slice(),
            [
                "restore",
                "--pathspec-from-file=pathspecs",
                "dir/nested.txt",
            ]
            .as_slice(),
            [
                "restore",
                "--pathspec-from-file=pathspecs",
                "--no-pathspec-from-file",
                "dir/nested.txt",
            ]
            .as_slice(),
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &upstream, args);
            let actual = run_output(sley_testkit::sley_bin!(), &rust, args);
            assert_same_output(actual, expected, args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_accepted_noop_options_match_upstream_git() {
    let root = unique_temp_dir("restore-accepted-noop-options");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            git(repo, &["add", "file.txt"]);
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree content");
        }

        let args = [
            "restore",
            "--quiet",
            "--no-quiet",
            "--overlay",
            "--no-overlay",
            "--ignore-unmerged",
            "--no-ignore-unmerged",
            "--ignore-skip-worktree-bits",
            "--no-ignore-skip-worktree-bits",
            "--no-recurse-submodules",
            "file.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore accepted no-op options"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_staged_paths_from_head_match_upstream_git() {
    let root = unique_temp_dir("restore-staged");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
            git(repo, &["add", "file.txt", "new.txt", "dir/nested.txt"]);
        }

        let args = ["restore", "--staged", "file.txt", "new.txt", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            b"staged\n",
            "restore --staged changed worktree file content"
        );
        assert!(
            rust.join("new.txt").exists(),
            "restore --staged removed worktree-only new file"
        );
        assert!(
            !rust.join("dir/nested.txt").exists(),
            "restore --staged restored deleted worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --staged"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --staged"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_source_head_worktree_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-source-head-worktree");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            git(repo, &["add", "file.txt"]);
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree content");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            git(repo, &["add", "new.txt"]);
        }

        let args = ["restore", "--source=HEAD", "file.txt", "new.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "restore --source=HEAD file content differed"
        );
        assert!(
            !rust.join("new.txt").exists(),
            "restore --source=HEAD left added worktree file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --source=HEAD"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --source=HEAD"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_source_commit_worktree_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-source-commit-worktree");
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
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree file");
            fs::write(repo.join("new.txt"), b"worktree new\n").expect("write worktree new");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let source_arg = format!("--source={target}");
        let args = ["restore", source_arg.as_str(), "file.txt", "new.txt", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "restore --source=<commit> file content differed"
        );
        assert_eq!(
            rust.join("new.txt").exists(),
            upstream.join("new.txt").exists(),
            "restore --source=<commit> new file presence differed"
        );
        assert_eq!(
            fs::read(rust.join("dir/nested.txt")).expect("read rust nested"),
            fs::read(upstream.join("dir/nested.txt")).expect("read upstream nested"),
            "restore --source=<commit> nested file differed"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --source=<commit>"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --source=<commit>"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_source_commit_staged_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-source-commit-staged");
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
            fs::write(repo.join("new.txt"), b"staged new\n").expect("write staged new");
            git(repo, &["add", "file.txt", "new.txt"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = [
            "restore",
            "--source",
            target.as_str(),
            "--staged",
            "file.txt",
            "new.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            b"staged\n",
            "restore --source=<commit> --staged changed worktree file"
        );
        assert_eq!(
            fs::read(rust.join("new.txt")).expect("read rust new"),
            b"staged new\n",
            "restore --source=<commit> --staged changed worktree new file"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --source=<commit> --staged"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --source=<commit> --staged"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_source_commit_staged_and_worktree_paths_match_upstream_git() {
    let root = unique_temp_dir("restore-source-commit-staged-worktree");
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
            fs::write(repo.join("file.txt"), b"worktree\n").expect("write worktree file");
            fs::write(repo.join("new.txt"), b"worktree new\n").expect("write worktree new");
            git(repo, &["add", "file.txt", "new.txt"]);
        }

        let target = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~1"]))
            .expect("target oid utf8")
            .trim()
            .to_string();
        let args = [
            "restore",
            "-s",
            target.as_str(),
            "-SW",
            "file.txt",
            "new.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "restore --source=<commit> --staged --worktree file content differed"
        );
        assert_eq!(
            rust.join("new.txt").exists(),
            upstream.join("new.txt").exists(),
            "restore --source=<commit> --staged --worktree new file presence differed"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --source=<commit> --staged --worktree"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --source=<commit> --staged --worktree"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn restore_staged_and_worktree_paths_from_head_match_upstream_git() {
    let root = unique_temp_dir("restore-staged-worktree");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"staged\n").expect("write staged content");
            fs::write(repo.join("new.txt"), b"new\n").expect("write new file");
            fs::remove_file(repo.join("dir/nested.txt")).expect("remove nested file");
            git(repo, &["add", "file.txt", "new.txt", "dir/nested.txt"]);
        }

        let args = ["restore", "-SW", "file.txt", "new.txt", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("file.txt")).expect("read rust file"),
            fs::read(upstream.join("file.txt")).expect("read upstream file"),
            "restore --staged --worktree file content differed"
        );
        assert!(
            !rust.join("new.txt").exists(),
            "restore --staged --worktree left added worktree file"
        );
        assert_eq!(
            fs::read(rust.join("dir/nested.txt")).expect("read rust nested file"),
            fs::read(upstream.join("dir/nested.txt")).expect("read upstream nested file"),
            "restore --staged --worktree nested file content differed"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after restore --staged --worktree"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after restore --staged --worktree"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

/// t2071: `restore -p --source=HEAD` (and `@`) discards worktree hunks only.
/// Post-apply index refresh must not re-stage the restored worktree content
/// over a divergent staged blob (matrix regression: 15→10).
#[test]
fn restore_patch_source_head_preserves_divergent_index() {
    let root = unique_temp_dir("restore-patch-source-head");
    fs::create_dir_all(&root).expect("create root");
    {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/foo"), b"parent\n").expect("write parent");
        fs::write(repo.join("bar"), b"dummy\n").expect("write bar");
        git(&repo, &["add", "bar", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "initial", "-q"]);
        fs::write(repo.join("dir/foo"), b"head\n").expect("write head");
        git(&repo, &["add", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "second", "-q"]);

        // worktree=work, index=index (divergent from HEAD=head)
        fs::write(repo.join("dir/foo"), b"index\n").expect("stage index");
        git(&repo, &["add", "dir/foo"]);
        fs::write(repo.join("dir/foo"), b"work\n").expect("dirty worktree");
        // bar also dirty so path ordering exercises skip+apply like upstream.
        fs::write(repo.join("bar"), b"bar_index\n").expect("bar index");
        git(&repo, &["add", "bar"]);
        fs::write(repo.join("bar"), b"bar_work\n").expect("bar work");

        for source in ["HEAD", "@"] {
            fs::write(repo.join("dir/foo"), b"index\n").expect("reset index content");
            git(&repo, &["add", "dir/foo"]);
            fs::write(repo.join("dir/foo"), b"work\n").expect("reset work");
            fs::write(repo.join("bar"), b"bar_index\n").expect("reset bar index");
            git(&repo, &["add", "bar"]);
            fs::write(repo.join("bar"), b"bar_work\n").expect("reset bar work");

            let source_arg = format!("--source={source}");
            let args = ["restore", "-p", source_arg.as_str()];
            // n = skip bar, y = discard dir/foo worktree hunk; extra n if loop continues
            let output = run_with_input(sley_testkit::sley_bin!(), &repo, &args, b"n\ny\nn\n");
            assert!(
                output.status.success(),
                "restore -p {source_arg} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Discard"),
                "expected Discard prompt in stdout for {source_arg}, got:\n{stdout}"
            );
            assert_eq!(
                fs::read(repo.join("dir/foo")).expect("read worktree"),
                b"head\n",
                "worktree should restore to HEAD content for {source_arg}"
            );
            assert_eq!(
                git(&repo, &["show", ":dir/foo"]),
                b"index\n",
                "index must stay divergent (not re-staged) for {source_arg}"
            );
            assert_eq!(
                fs::read(repo.join("bar")).expect("read bar"),
                b"bar_work\n",
                "skipped bar worktree must be unchanged for {source_arg}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

/// t2071 #8/#13 family: `restore -p --source=HEAD^` / path-limited source.
#[test]
fn restore_patch_source_parent_and_path_limit() {
    let root = unique_temp_dir("restore-patch-source-parent");
    fs::create_dir_all(&root).expect("create root");
    {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/foo"), b"parent\n").expect("write parent");
        fs::write(repo.join("bar"), b"dummy\n").expect("write bar");
        git(&repo, &["add", "bar", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "initial", "-q"]);
        fs::write(repo.join("dir/foo"), b"head\n").expect("write head");
        git(&repo, &["add", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "second", "-q"]);

        // --source=HEAD^: worktree→parent, index stays index
        fs::write(repo.join("dir/foo"), b"index\n").expect("stage index");
        git(&repo, &["add", "dir/foo"]);
        fs::write(repo.join("dir/foo"), b"work\n").expect("dirty worktree");
        fs::write(repo.join("bar"), b"bar_index\n").expect("bar index");
        git(&repo, &["add", "bar"]);
        fs::write(repo.join("bar"), b"bar_work\n").expect("bar work");

        let output = run_with_input(
            sley_testkit::sley_bin!(),
            &repo,
            &["restore", "-p", "--source=HEAD^"],
            b"n\ny\nn\n",
        );
        assert!(
            output.status.success(),
            "restore -p --source=HEAD^ failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(repo.join("dir/foo")).expect("read worktree"),
            b"parent\n"
        );
        assert_eq!(git(&repo, &["show", ":dir/foo"]), b"index\n");

        // path limiting: HEAD^ -- dir
        fs::write(repo.join("dir/foo"), b"head\n").expect("reset index to head");
        git(&repo, &["add", "dir/foo"]);
        fs::write(repo.join("dir/foo"), b"work\n").expect("dirty worktree");
        fs::write(repo.join("bar"), b"bar_index\n").expect("bar index");
        git(&repo, &["add", "bar"]);
        fs::write(repo.join("bar"), b"bar_work\n").expect("bar work");

        let output = run_with_input(
            sley_testkit::sley_bin!(),
            &repo,
            &["restore", "-p", "--source=HEAD^", "--", "dir"],
            b"y\nn\nn\n",
        );
        assert!(
            output.status.success(),
            "restore -p --source=HEAD^ -- dir failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(repo.join("dir/foo")).expect("read worktree"),
            b"parent\n"
        );
        assert_eq!(
            git(&repo, &["show", ":dir/foo"]),
            b"head\n",
            "path-limited restore must not re-stage dir/foo"
        );
        assert_eq!(
            fs::read(repo.join("bar")).expect("read bar"),
            b"bar_work\n",
            "path-limited restore must not touch bar"
        );
    };
    let _ = fs::remove_dir_all(&root);
}
