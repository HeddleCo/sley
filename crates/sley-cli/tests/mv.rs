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
fn mv_tracked_file_matches_upstream_git() {
    let root = unique_temp_dir("mv-tracked-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["mv", "file.txt", "renamed.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert!(!rust.join("file.txt").exists(), "sley mv left source");
        assert_eq!(
            fs::read(rust.join("renamed.txt")).expect("read renamed file"),
            b"base\n",
            "renamed file content differed"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after mv"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after mv"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_sha256_tracked_file_matches_upstream_git() {
    let root = unique_temp_dir("mv-sha256");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_sha256_repo(&upstream);
        prepare_sha256_repo(&rust);

        let args = ["mv", "file.txt", "renamed.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            upstream.join("file.txt").exists(),
            rust.join("file.txt").exists(),
            "source presence differed"
        );
        assert_eq!(
            upstream.join("renamed.txt").exists(),
            rust.join("renamed.txt").exists(),
            "destination presence differed"
        );
        for args in [
            vec!["diff", "--cached", "--name-status"],
            vec!["status", "--short"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
            let actual = run_output(sley_testkit::oracle_git(), &rust, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_modified_file_matches_upstream_git() {
    let root = unique_temp_dir("mv-modified-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        for repo in [&upstream, &rust] {
            fs::write(repo.join("file.txt"), b"modified\n").expect("modify file");
        }

        let args = ["mv", "file.txt", "renamed.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            fs::read(rust.join("renamed.txt")).expect("read renamed file"),
            b"modified\n",
            "renamed modified file content differed"
        );
        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after modified mv"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after modified mv"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_accepted_noop_options_match_upstream_git() {
    let root = unique_temp_dir("mv-accepted-noop-options");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = [
            "mv",
            "--no-force",
            "-k",
            "--no-dry-run",
            "--no-verbose",
            "--sparse",
            "--no-sparse",
            "file.txt",
            "renamed.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after mv accepted no-op options"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_dry_run_and_verbose_match_upstream_git() {
    let root = unique_temp_dir("mv-dry-run-verbose");
    fs::create_dir_all(&root).expect("create root");
    for (idx, (args, moves_path)) in [
        (vec!["mv", "--dry-run", "file.txt", "renamed.txt"], false),
        (
            vec!["mv", "-n", "--no-dry-run", "file.txt", "renamed.txt"],
            true,
        ),
        (vec!["mv", "--verbose", "file.txt", "renamed.txt"], true),
        (
            vec!["mv", "-v", "--no-verbose", "file.txt", "renamed.txt"],
            true,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            rust.join("file.txt").exists(),
            !moves_path,
            "source path state differed for {args:?}"
        );
        assert_eq!(
            rust.join("renamed.txt").exists(),
            moves_path,
            "destination path state differed for {args:?}"
        );
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_multiple_sources_to_directory_match_upstream_git() {
    let root = unique_temp_dir("mv-multiple-sources");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "first.txt", "second.txt", "dir"],
        vec!["mv", "--verbose", "first.txt", "second.txt", "dir"],
        vec!["mv", "--dry-run", "first.txt", "second.txt", "dir"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("first.txt"), b"first\n").expect("write first");
            fs::write(repo.join("second.txt"), b"second\n").expect("write second");
            fs::create_dir(repo.join("dir")).expect("create destination directory");
            git(repo, &["add", "first.txt", "second.txt"]);
            run_with_identity(repo, &["commit", "-m", "multi", "-q"]);
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_tracked_directory_matches_upstream_git() {
    let root = unique_temp_dir("mv-tracked-directory");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "src", "dst"],
        vec!["mv", "--verbose", "src", "dst"],
        vec!["mv", "--dry-run", "src", "dst"],
        vec!["mv", "src", "existing"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::create_dir_all(repo.join("src/sub")).expect("create source directory");
            fs::write(repo.join("src/a.txt"), b"first\n").expect("write first");
            fs::write(repo.join("src/sub/b.txt"), b"second\n").expect("write second");
            fs::create_dir(repo.join("existing")).expect("create existing directory");
            git(repo, &["add", "src"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
        assert_eq!(
            git(&rust, &["ls-files"]),
            git(&upstream, &["ls-files"]),
            "index paths differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_destination_error_paths_match_upstream_git() {
    let root = unique_temp_dir("mv-destination-errors");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "file.txt", "missing/"],
        vec!["mv", "file.txt", "missing/sub"],
        vec!["mv", "-k", "file.txt", "missing/"],
        vec!["mv", "-k", "file.txt", "missing/sub"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_dry_run_error_paths_match_upstream_git() {
    let root = unique_temp_dir("mv-dry-run-errors");
    fs::create_dir_all(&root).expect("create root");
    for (idx, (args, add_destination)) in [
        (vec!["mv", "--dry-run", "file.txt", "missing/"], false),
        (vec!["mv", "--dry-run", "missing.txt", "renamed.txt"], false),
        (vec!["mv", "--dry-run", "file.txt", "dest.txt"], true),
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);
        if add_destination {
            for repo in [&upstream, &rust] {
                fs::write(repo.join("dest.txt"), b"dest\n").expect("write destination");
                git(repo, &["add", "dest.txt"]);
                run_with_identity(repo, &["commit", "-m", "dest", "-q"]);
            }
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_bad_source_errors_match_upstream_git() {
    let root = unique_temp_dir("mv-bad-source-errors");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "missing.txt", "renamed.txt"],
        vec!["mv", "missing.txt", "file.txt", "dir"],
        vec!["mv", "untracked.txt", "renamed.txt"],
        vec!["mv", "untracked.txt", "file.txt", "dir"],
        vec!["mv", "--dry-run", "untracked.txt", "renamed.txt"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::create_dir(repo.join("dir")).expect("create destination directory");
            fs::write(repo.join("untracked.txt"), b"untracked\n").expect("write untracked");
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_skip_errors_matches_upstream_git() {
    let root = unique_temp_dir("mv-skip-errors");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "-k", "missing.txt", "file.txt", "dir"],
        vec!["mv", "-k", "--verbose", "missing.txt", "file.txt", "dir"],
        vec!["mv", "-k", "--dry-run", "missing.txt", "file.txt", "dir"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::create_dir(repo.join("dir")).expect("create destination directory");
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn mv_combined_short_options_match_upstream_git() {
    let root = unique_temp_dir("mv-combined-short-options");
    fs::create_dir_all(&root).expect("create root");
    for (idx, args) in [
        vec!["mv", "-vn", "file.txt", "renamed.txt"],
        vec!["mv", "-nv", "file.txt", "renamed.txt"],
        vec!["mv", "-kf", "missing.txt", "file.txt", "dir"],
    ]
    .into_iter()
    .enumerate()
    {
        let upstream = root.join(format!("upstream-{idx}"));
        let rust = root.join(format!("rust-{idx}"));
        fs::create_dir_all(&upstream).expect("create upstream repo");
        fs::create_dir_all(&rust).expect("create rust repo");
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::create_dir(repo.join("dir")).expect("create destination directory");
        }

        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_sley"), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
