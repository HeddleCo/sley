use std::fs;
use std::os::unix::fs::PermissionsExt;
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

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
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
    fs::create_dir_all(root.join("dir")).expect("create dir");
    fs::write(root.join("file.txt"), b"base\n").expect("write file");
    fs::write(root.join("dir/nested.txt"), b"nested\n").expect("write nested");
}

fn prepare_tracked_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("tracked.txt"), b"base\n").expect("write tracked file");
    fs::write(root.join("gone.txt"), b"gone\n").expect("write deleted fixture");
    git(root, &["add", "tracked.txt", "gone.txt"]);
    run_with_identity(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("tracked.txt"), b"modified\n").expect("modify tracked file");
    fs::remove_file(root.join("gone.txt")).expect("delete tracked file");
    fs::write(root.join("new.txt"), b"new\n").expect("write untracked file");
}

fn make_executable(path: &Path) {
    let mut permissions = fs::metadata(path)
        .expect("read executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable bit");
}

#[test]
fn add_dry_run_reports_without_staging_like_upstream_git() {
    let root = unique_temp_dir("add-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-n", "file.txt", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add -n"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_with_explicit_git_dir_uses_invocation_cwd_as_worktree() {
    let root = unique_temp_dir("add-explicit-git-dir-cwd");
    let upstream_repo = root.join("upstream-repo");
    let upstream_worktree = root.join("upstream-input");
    let rust_repo = root.join("rust-repo");
    let rust_worktree = root.join("rust-input");
    for path in [
        &upstream_repo,
        &upstream_worktree,
        &rust_repo,
        &rust_worktree,
    ] {
        fs::create_dir_all(path).expect("create fixture directory");
    }
    prepare_repo(&upstream_repo);
    prepare_repo(&rust_repo);
    fs::write(upstream_worktree.join("fixture.txt"), b"fixture\n").expect("write fixture");
    fs::write(rust_worktree.join("fixture.txt"), b"fixture\n").expect("write fixture");

    let upstream_git_dir = format!("--git-dir={}", upstream_repo.join(".git").display());
    let rust_git_dir = format!("--git-dir={}", rust_repo.join(".git").display());
    let expected = run_output(
        sley_testkit::oracle_git(),
        &upstream_worktree,
        &[&upstream_git_dir, "add", "."],
    );
    let actual = run_output(
        sley_testkit::sley_bin!(),
        &rust_worktree,
        &[&rust_git_dir, "add", "."],
    );
    assert_same_output(actual, expected, &["--git-dir=<separate>", "add", "."]);

    let expected_index = run_output(
        sley_testkit::oracle_git(),
        &upstream_worktree,
        &[&upstream_git_dir, "ls-files", "--stage"],
    );
    let actual_index = run_output(
        sley_testkit::sley_bin!(),
        &rust_worktree,
        &[&rust_git_dir, "ls-files", "--stage"],
    );
    assert_same_output(
        actual_index,
        expected_index,
        &["--git-dir=<separate>", "ls-files", "--stage"],
    );

    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn add_verbose_stages_and_reports_directory_paths_like_upstream_git() {
    let root = unique_temp_dir("add-verbose");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-v", "dir", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add -v"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_combined_dry_run_verbose_matches_upstream_git() {
    let root = unique_temp_dir("add-combined-dry-run-verbose");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-nv", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add -nv"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_dry_run_overrides_dry_run_like_upstream_git() {
    let root = unique_temp_dir("add-no-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-n", "--no-dry-run", "-v", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --no-dry-run"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_verbose_overrides_verbose_like_upstream_git() {
    let root = unique_temp_dir("add-no-verbose");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-v", "--no-verbose", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --no-verbose"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_update_overrides_update_like_upstream_git() {
    let root = unique_temp_dir("add-no-update");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-u", "--no-update", "-v", "new.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add --no-update"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_ignore_missing_dry_run_skips_missing_pathspec_like_upstream_git() {
    let root = unique_temp_dir("add-ignore-missing");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-n", "--ignore-missing", "missing.txt", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --ignore-missing"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_ignore_missing_requires_dry_run_like_upstream_git() {
    let root = unique_temp_dir("add-ignore-missing-without-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "--ignore-missing", "missing.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_ignore_missing_restores_missing_pathspec_error_like_upstream_git() {
    let root = unique_temp_dir("add-no-ignore-missing");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = [
            "add",
            "-n",
            "--ignore-missing",
            "--no-ignore-missing",
            "missing.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_path_stages_tracked_deletion_like_upstream_git() {
    let root = unique_temp_dir("add-path-deletion");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("gone.txt"), b"gone\n").expect("write deleted fixture");
            git(repo, &["add", "gone.txt"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::remove_file(repo.join("gone.txt")).expect("delete tracked file");
        }

        let args = ["add", "gone.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add deleted path"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_directory_stages_tracked_deletions_like_upstream_git() {
    let root = unique_temp_dir("add-directory-deletion");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::create_dir_all(repo.join("dir")).expect("create dir");
            fs::write(repo.join("dir/keep.txt"), b"base\n").expect("write kept file");
            fs::write(repo.join("dir/gone.txt"), b"gone\n").expect("write deleted fixture");
            git(repo, &["add", "dir/keep.txt", "dir/gone.txt"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::write(repo.join("dir/keep.txt"), b"modified\n").expect("modify tracked file");
            fs::remove_file(repo.join("dir/gone.txt")).expect("delete tracked file");
        }

        let args = ["add", "-nv", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        let args = ["add", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add directory with deleted path"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_clean_path_dry_run_is_quiet_like_upstream_git() {
    let root = unique_temp_dir("add-clean-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("clean.txt"), b"clean\n").expect("write clean file");
            git(repo, &["add", "clean.txt"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        }

        let args = ["add", "-nv", "clean.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_touched_tracked_path_restats_without_verbose_action_like_upstream_git() {
    let root = unique_temp_dir("add-touched-tracked");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("tracked.txt"), b"same\n").expect("write tracked");
            git(repo, &["add", "tracked.txt"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
        for repo in [&upstream, &rust] {
            fs::write(repo.join("tracked.txt"), b"same\n").expect("touch tracked content");
        }

        let args = ["add", "-v", "tracked.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            run(
                sley_testkit::sley_bin!(),
                &rust,
                &["diff-files", "--name-only"]
            ),
            git(&upstream, &["diff-files", "--name-only"]),
            "diff-files differed after restatting touched tracked path"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_exact_tracked_path_applies_attributes_like_upstream_git() {
    let root = unique_temp_dir("add-exact-tracked-attrs");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            git(repo, &["config", "filter.upper.clean", "tr a-z A-Z"]);
            fs::write(repo.join(".gitattributes"), b"*.dat filter=upper\n")
                .expect("write attributes");
            fs::write(repo.join("tracked.dat"), b"base\n").expect("write tracked");
            git(repo, &["add", ".gitattributes", "tracked.dat"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::write(repo.join("tracked.dat"), b"mixed content\n").expect("modify tracked");
        }

        let args = ["add", "-v", "tracked.dat"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["show", ":tracked.dat"]),
            git(&upstream, &["show", ":tracked.dat"]),
            "staged blob differed after add with attributes"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_missing_pathspec_matches_upstream_git() {
    let root = unique_temp_dir("add-missing-pathspec");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
        }

        let args = ["add", "missing.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_path_inside_submodule_is_rejected_like_upstream_git() {
    let root = unique_temp_dir("add-path-inside-submodule");
    let sub_src = root.join("sub-src");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&sub_src).expect("create submodule source");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&sub_src, &["init", "-q", "-b", "main"]);
        fs::write(sub_src.join("inside.txt"), b"inside\n").expect("write submodule file");
        git(&sub_src, &["add", "inside.txt"]);
        run_with_identity(&sub_src, &["commit", "-m", "sub", "-q"]);

        let sub_src_arg = sub_src.to_string_lossy().into_owned();
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            git(
                repo,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    &sub_src_arg,
                    "sub",
                ],
            );
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::write(repo.join("sub/inside.txt"), b"changed\n").expect("modify submodule file");
        }

        let args = ["add", "sub/inside.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_ignore_removal_stages_adds_and_modifications_only_like_upstream_git() {
    let root = unique_temp_dir("add-ignore-removal");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "--ignore-removal", "-v", "."];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add --ignore-removal"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_ignore_removal_dry_run_leaves_deletions_unstaged_like_upstream_git() {
    let root = unique_temp_dir("add-ignore-removal-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "--ignore-removal", "-n", "."];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add --ignore-removal -n"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_all_alias_matches_upstream_git() {
    let root = unique_temp_dir("add-no-all");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "--no-all", "-v", "."];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add --no-all"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_ignore_removal_explicit_deleted_path_is_noop_like_upstream_git() {
    let root = unique_temp_dir("add-ignore-removal-deleted-path");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("gone.txt"), b"gone\n").expect("write deleted fixture");
            git(repo, &["add", "gone.txt"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::remove_file(repo.join("gone.txt")).expect("delete tracked file");
        }

        let args = ["add", "--ignore-removal", "gone.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add --ignore-removal deleted path"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_update_stages_tracked_modifications_and_deletions_like_upstream_git() {
    let root = unique_temp_dir("add-update");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-u"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -u"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_update_verbose_reports_tracked_actions_like_upstream_git() {
    let root = unique_temp_dir("add-update-verbose");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-uv"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -uv"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_update_dry_run_reports_without_staging_like_upstream_git() {
    let root = unique_temp_dir("add-update-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-un"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -un"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_stages_tracked_and_untracked_changes_like_upstream_git() {
    let root = unique_temp_dir("add-all");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-A"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -A"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_verbose_reports_all_actions_like_upstream_git() {
    let root = unique_temp_dir("add-all-verbose");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-Av"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -Av"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_dry_run_reports_without_staging_like_upstream_git() {
    let root = unique_temp_dir("add-all-dry-run");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_tracked_repo(&upstream);
        prepare_tracked_repo(&rust);

        let args = ["add", "-An"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -An"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_pathspec_limits_actions_like_upstream_git() {
    let root = unique_temp_dir("add-all-pathspec");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::create_dir_all(repo.join("dir")).expect("create dir");
            fs::create_dir_all(repo.join("other")).expect("create other dir");
            fs::write(repo.join("dir/tracked.txt"), b"base\n").expect("write tracked");
            fs::write(repo.join("dir/gone.txt"), b"gone\n").expect("write gone");
            fs::write(repo.join("other/tracked.txt"), b"other\n").expect("write other");
            git(
                repo,
                &[
                    "add",
                    "dir/tracked.txt",
                    "dir/gone.txt",
                    "other/tracked.txt",
                ],
            );
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
            fs::write(repo.join("dir/tracked.txt"), b"modified\n").expect("modify tracked");
            fs::remove_file(repo.join("dir/gone.txt")).expect("delete gone");
            fs::write(repo.join("dir/new.txt"), b"new\n").expect("write new");
            fs::write(repo.join("other/tracked.txt"), b"other modified\n").expect("modify other");
            fs::write(repo.join("root.txt"), b"root\n").expect("write root");
        }

        let args = ["add", "-Av", "dir"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["status", "--short"]),
            git(&upstream, &["status", "--short"]),
            "status differed after add -Av dir"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_missing_pathspec_matches_upstream_git() {
    let root = unique_temp_dir("add-all-missing-pathspec");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-A", "missing.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_all_ignore_missing_dry_run_skips_missing_pathspec_like_upstream_git() {
    let root = unique_temp_dir("add-all-ignore-missing");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "-n", "--ignore-missing", "-A", "missing.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_chmod_stages_executable_bit_like_upstream_git() {
    let root = unique_temp_dir("add-chmod-plus-x");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("run.sh"), b"run\n").expect("write script");
        }

        let args = ["add", "--chmod=+x", "run.sh"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["ls-files", "--stage"]),
            git(&upstream, &["ls-files", "--stage"]),
            "index modes differed after add --chmod=+x"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_chmod_removes_executable_bit_like_upstream_git() {
    let root = unique_temp_dir("add-chmod-minus-x");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("run.sh"), b"run\n").expect("write script");
            make_executable(&repo.join("run.sh"));
            git(repo, &["add", "run.sh"]);
            run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        }

        let args = ["add", "--chmod=-x", "run.sh"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--summary"]),
            git(&upstream, &["diff", "--cached", "--summary"]),
            "cached summary differed after add --chmod=-x"
        );
        assert_eq!(
            git(&rust, &["ls-files", "--stage"]),
            git(&upstream, &["ls-files", "--stage"]),
            "index modes differed after add --chmod=-x"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_chmod_invalid_value_matches_upstream_git() {
    let root = unique_temp_dir("add-chmod-invalid");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("run.sh"), b"run\n").expect("write script");
        }

        let args = ["add", "--chmod=bad", "run.sh"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_chmod_overrides_chmod_like_upstream_git() {
    let root = unique_temp_dir("add-no-chmod");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        for repo in [&upstream, &rust] {
            git(repo, &["init", "-q", "-b", "main"]);
            fs::write(repo.join("run.sh"), b"run\n").expect("write script");
        }

        let args = ["add", "--chmod=+x", "--no-chmod", "run.sh"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["ls-files", "--stage"]),
            git(&upstream, &["ls-files", "--stage"]),
            "index modes differed after add --no-chmod"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_sparse_and_ignore_errors_flags_are_accepted_like_upstream_git() {
    let root = unique_temp_dir("add-noop-flags");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = [
            "add",
            "--sparse",
            "--no-sparse",
            "--ignore-errors",
            "--no-ignore-errors",
            "file.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add sparse/ignore-errors flags"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_pathspec_from_file_matches_upstream_git() {
    let root = unique_temp_dir("add-pathspec-from-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\ndir/nested.txt\n")
                .expect("write pathspec file");
        }

        let args = ["add", "--pathspec-from-file=pathspecs"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --pathspec-from-file"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_pathspec_file_nul_matches_upstream_git() {
    let root = unique_temp_dir("add-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\0dir/nested.txt\0")
                .expect("write pathspec file");
        }

        let args = [
            "add",
            "--pathspec-file-nul",
            "--pathspec-from-file",
            "pathspecs",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --pathspec-file-nul"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_pathspec_file_nul_overrides_previous_value_like_upstream_git() {
    let root = unique_temp_dir("add-no-pathspec-file-nul");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("pathspecs"), b"file.txt\ndir/nested.txt\n")
                .expect("write pathspec file");
        }

        let args = [
            "add",
            "--pathspec-file-nul",
            "--no-pathspec-file-nul",
            "--pathspec-from-file=pathspecs",
            "-v",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);

        assert_eq!(
            git(&rust, &["diff", "--cached", "--name-status"]),
            git(&upstream, &["diff", "--cached", "--name-status"]),
            "cached diff differed after add --no-pathspec-file-nul"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_no_pathspec_from_file_keeps_inline_rejection_like_upstream_git() {
    let root = unique_temp_dir("add-no-pathspec-from-file");
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

        let args = [
            "add",
            "--pathspec-from-file=pathspecs",
            "--no-pathspec-from-file",
            "dir/nested.txt",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_pathspec_from_file_rejects_inline_pathspecs_like_upstream_git() {
    let root = unique_temp_dir("add-pathspec-from-file-mixed");
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

        let args = ["add", "--pathspec-from-file=pathspecs", "dir/nested.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn add_pathspec_file_nul_requires_pathspec_from_file_like_upstream_git() {
    let root = unique_temp_dir("add-pathspec-file-nul-without-file");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["add", "--pathspec-file-nul", "file.txt"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
