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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_env(program: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
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
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin)
        .expect("write stdin");
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
        "status differed for {args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stdout),
        String::from_utf8_lossy(&expected.stderr)
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

fn assert_same_output_with_normalized_stderr(
    actual: Output,
    expected: Output,
    args: &[&str],
    actual_repo: &Path,
    expected_repo: &Path,
) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}\nactual stdout:\n{}\nactual stderr:\n{}\nexpected stdout:\n{}\nexpected stderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stdout),
        String::from_utf8_lossy(&expected.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    let actual_stderr = normalize_repo_path_in_output(&actual.stderr, actual_repo);
    let expected_stderr = normalize_repo_path_in_output(&expected.stderr, expected_repo);
    assert_eq!(
        actual_stderr, expected_stderr,
        "stderr differed for {args:?}"
    );
}

fn normalize_repo_path_in_output(output: &[u8], repo: &Path) -> Vec<u8> {
    let output = String::from_utf8_lossy(output);
    let canonical = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    output
        .replace(&canonical.to_string_lossy().to_string(), "<repo>")
        .replace(&repo.to_string_lossy().to_string(), "<repo>")
        .into_bytes()
}

fn reset_fixture(root: &Path) {
    let _ = fs::remove_file(root.join("one.txt"));
    let _ = fs::remove_file(root.join("keep.txt"));
    let _ = fs::remove_file(root.join("new.txt"));
    let _ = fs::remove_file(root.join("z.txt"));
    let _ = fs::remove_file(root.join("--add"));
    run_success(sley_testkit::oracle_git(), root, &["reset", "-q", "--mixed"]);
    fs::write(root.join("one.txt"), b"one").expect("write one");
    fs::write(root.join("keep.txt"), b"keep").expect("write keep");
    run_success(sley_testkit::oracle_git(), root, &["add", "one.txt", "keep.txt"]);
    fs::write(root.join("keep.txt"), b"changed").expect("modify keep");
    fs::remove_file(root.join("one.txt")).expect("remove one");
    fs::write(root.join("new.txt"), b"new").expect("write new");
    fs::write(root.join("z.txt"), b"z").expect("write z");
    fs::write(root.join("--add"), b"dash add").expect("write option-like path");
}

fn reset_clean_fixture(root: &Path) {
    let _ = fs::remove_file(root.join("one.txt"));
    let _ = fs::remove_file(root.join("keep.txt"));
    run_success(sley_testkit::oracle_git(), root, &["reset", "-q", "--mixed"]);
    fs::write(root.join("one.txt"), b"one").expect("write one");
    fs::write(root.join("keep.txt"), b"keep").expect("write keep");
    run_success(sley_testkit::oracle_git(), root, &["add", "one.txt", "keep.txt"]);
}

fn assert_index_matches(expected: &Path, actual: &Path, args: &[&str]) {
    let expected_index = run_success(sley_testkit::oracle_git(), expected, &["ls-files", "--stage"]);
    let actual_index = run_success(sley_testkit::oracle_git(), actual, &["ls-files", "--stage"]);
    assert_eq!(
        actual_index, expected_index,
        "index differed after {args:?}"
    );
}

fn assert_index_matches_for_label(expected: &Path, actual: &Path, label: &str) {
    let expected_index = run_success(sley_testkit::oracle_git(), expected, &["ls-files", "--stage"]);
    let actual_index = run_success(sley_testkit::oracle_git(), actual, &["ls-files", "--stage"]);
    assert_eq!(actual_index, expected_index, "index differed after {label}");
}

fn assert_ls_files_verbose_matches(expected: &Path, actual: &Path, label: &str) {
    let expected_output = run_success(sley_testkit::oracle_git(), expected, &["ls-files", "-v"]);
    let actual_output = run_success(sley_testkit::oracle_git(), actual, &["ls-files", "-v"]);
    assert_eq!(
        actual_output, expected_output,
        "ls-files -v differed after {label}"
    );
}

fn assert_index_version_matches(expected: &Path, actual: &Path, label: &str) {
    let expected_output = run_success(sley_testkit::oracle_git(), expected, &["update-index", "--show-index-version"]);
    let actual_output = run_success(sley_testkit::oracle_git(), actual, &["update-index", "--show-index-version"]);
    assert_eq!(
        actual_output, expected_output,
        "index version differed after {label}"
    );
}

fn index_oid_for_path(root: &Path, path: &str) -> String {
    let output = run_success(sley_testkit::oracle_git(), root, &["ls-files", "--stage"]);
    let output = String::from_utf8(output).expect("stage output is utf8");
    output
        .lines()
        .find_map(|line| {
            let (metadata, entry_path) = line.split_once('\t')?;
            if entry_path == path {
                metadata.split_whitespace().nth(1).map(ToOwned::to_owned)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("missing staged path {path} in:\n{output}"))
}

fn assert_object_existence_matches(expected: &Path, actual: &Path, path: &str, label: &str) {
    let expected_oid = index_oid_for_path(expected, path);
    let actual_oid = index_oid_for_path(actual, path);
    assert_eq!(
        actual_oid, expected_oid,
        "oid differed for {path} after {label}"
    );
    let args = ["cat-file", "-e", expected_oid.as_str()];
    let expected_output = run(sley_testkit::oracle_git(), expected, &args);
    let actual_output = run(sley_testkit::oracle_git(), actual, &args);
    assert_same_output(actual_output, expected_output, &args);
}

#[test]
fn update_index_path_modes_match_upstream_git() {
    let root = unique_temp_dir("update-index-path-modes");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "keep.txt"],
            vec!["update-index", "-q", "keep.txt"],
            vec!["update-index", "--no-verbose", "keep.txt"],
            vec![
                "update-index",
                "--ignore-submodules",
                "--no-ignore-submodules",
                "keep.txt",
            ],
            vec!["update-index", "--replace", "--no-replace", "keep.txt"],
            vec!["update-index", "--unmerged", "--no-unmerged", "keep.txt"],
            vec!["update-index", "--no-index-version", "keep.txt"],
            vec!["update-index", "--no-split-index", "keep.txt"],
            vec!["update-index", "--no-untracked-cache", "keep.txt"],
            vec!["update-index", "--no-test-untracked-cache", "keep.txt"],
            vec!["update-index", "--no-force-untracked-cache", "keep.txt"],
            vec!["update-index", "--no-fsmonitor", "keep.txt"],
            vec![
                "update-index",
                "--ignore-skip-worktree-entries",
                "--no-ignore-skip-worktree-entries",
                "keep.txt",
            ],
            vec![
                "update-index",
                "--force-write-index",
                "--no-force-write-index",
                "keep.txt",
            ],
            vec!["update-index", "--add", "new.txt"],
            vec!["update-index", "--add", "--no-add", "new.txt"],
            vec!["update-index", "--no-add", "--add", "new.txt"],
            vec!["update-index", "--remove", "one.txt"],
            vec!["update-index", "--remove", "--no-remove", "one.txt"],
            vec!["update-index", "--no-remove", "--remove", "one.txt"],
            vec!["update-index", "--remove", "missing.txt"],
            vec!["update-index", "--force-remove", "keep.txt"],
            vec![
                "update-index",
                "--force-remove",
                "--no-force-remove",
                "keep.txt",
            ],
            vec![
                "update-index",
                "--no-force-remove",
                "--force-remove",
                "keep.txt",
            ],
            vec!["update-index", "new.txt"],
            vec!["update-index", "one.txt"],
            vec!["update-index", "--chmod=+x", "keep.txt"],
            vec!["update-index", "--chmod", "+x", "keep.txt"],
            vec!["update-index", "--chmod=-x", "keep.txt"],
            vec!["update-index", "--"],
            vec!["update-index", "--add", "--", "--add"],
            vec!["update-index", "--", "--add"],
            vec!["update-index", "--chmod=+x", "--", "keep.txt"],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_no_input_compat_flags_match_upstream_git() {
    let root = unique_temp_dir("update-index-no-input-compat");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        let args_cases = [
            vec!["update-index", "--ignore-submodules"],
            vec!["update-index", "--no-ignore-submodules"],
            vec!["update-index", "--replace"],
            vec!["update-index", "--no-replace"],
            vec!["update-index", "--unmerged"],
            vec!["update-index", "--no-unmerged"],
            vec!["update-index", "--no-index-version"],
            vec!["update-index", "--no-split-index"],
            vec!["update-index", "--no-untracked-cache"],
            vec!["update-index", "--no-test-untracked-cache"],
            vec!["update-index", "--no-force-untracked-cache"],
            vec!["update-index", "--no-fsmonitor"],
            vec!["update-index", "--again"],
            vec!["update-index", "-g"],
            vec!["update-index", "--ignore-skip-worktree-entries"],
            vec!["update-index", "--no-ignore-skip-worktree-entries"],
            vec!["update-index", "--force-write-index"],
            vec!["update-index", "--no-force-write-index"],
            vec![
                "update-index",
                "--force-write-index",
                "--no-force-write-index",
            ],
            vec![
                "update-index",
                "--no-force-write-index",
                "--force-write-index",
            ],
        ];

        for args in args_cases.clone() {
            let _ = fs::remove_file(expected.join(".git").join("index"));
            let _ = fs::remove_file(actual.join(".git").join("index"));
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }

        for args in args_cases {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_git_index_file_matches_upstream_git() {
    let root = unique_temp_dir("update-index-git-index-file");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);
        for repo in [&expected, &actual] {
            fs::create_dir_all(repo.join("custom-index")).expect("create custom index dir");
            fs::write(repo.join("file.txt"), b"file\n").expect("write file");
        }
        let envs = [("GIT_INDEX_FILE", "custom-index/index")];

        let update_args = ["update-index", "--add", "file.txt"];
        let expected_output = run_with_env(sley_testkit::oracle_git(), &expected, &update_args, &envs);
        let actual_output = run_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &update_args, &envs);
        assert_same_output(actual_output, expected_output, &update_args);

        for args in [
            vec!["ls-files", "--stage"],
            vec!["update-index", "--show-index-version"],
            vec!["write-tree"],
        ] {
            let expected_output = run_with_env(sley_testkit::oracle_git(), &expected, &args, &envs);
            let actual_output = run_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &envs);
            assert_same_output(actual_output, expected_output, &args);
        }

        assert!(expected.join("custom-index").join("index").exists());
        assert!(actual.join("custom-index").join("index").exists());
        let default_args = ["ls-files", "--stage"];
        assert_same_output(
            run(env!("CARGO_BIN_EXE_sley"), &actual, &default_args),
            run(sley_testkit::oracle_git(), &expected, &default_args),
            &default_args,
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_test_untracked_cache_matches_upstream_git() {
    let root = unique_temp_dir("update-index-test-untracked-cache");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--test-untracked-cache"],
            vec![
                "update-index",
                "--test-untracked-cache",
                "--show-index-version",
            ],
            vec![
                "update-index",
                "--test-untracked-cache",
                "--no-test-untracked-cache",
            ],
            vec![
                "update-index",
                "--no-test-untracked-cache",
                "--test-untracked-cache",
            ],
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_with_normalized_stderr(
                actual_output,
                expected_output,
                &args,
                &actual,
                &expected,
            );
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_fsmonitor_matches_upstream_git_when_unset() {
    let root = unique_temp_dir("update-index-fsmonitor-unset");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--fsmonitor"],
            vec!["update-index", "--fsmonitor", "--show-index-version"],
            vec!["update-index", "--fsmonitor", "--no-fsmonitor"],
            vec!["update-index", "--no-fsmonitor", "--fsmonitor"],
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }

        for args in [
            vec!["update-index", "--fsmonitor", "keep.txt"],
            vec!["update-index", "keep.txt", "--fsmonitor"],
            vec!["update-index", "--fsmonitor", "--no-fsmonitor", "keep.txt"],
            vec!["update-index", "--no-fsmonitor", "--fsmonitor", "keep.txt"],
            vec!["update-index", "--fsmonitor", "absent.txt"],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
                assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_unresolve_no_resolve_undo_matches_upstream_git() {
    let root = unique_temp_dir("update-index-unresolve");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--unresolve"],
            vec!["update-index", "--unresolve", "--show-index-version"],
            vec!["update-index", "--unresolve", "missing.txt"],
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }

        for args in [
            vec!["update-index", "--unresolve", "keep.txt"],
            vec!["update-index", "keep.txt", "--unresolve"],
            vec!["update-index", "keep.txt", "--unresolve", "new.txt"],
            vec!["update-index", "--unresolve", "--add", "new.txt"],
            vec![
                "update-index",
                "keep.txt",
                "--unresolve",
                "--show-index-version",
            ],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_index_version_matches(&expected, &actual, &format!("{args:?}"));
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_again_matches_upstream_git() {
    let root = unique_temp_dir("update-index-again");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for repo in [&expected, &actual] {
            fs::write(repo.join("one.txt"), b"base-one").expect("write one");
            fs::write(repo.join("keep.txt"), b"base-keep").expect("write keep");
            run_success(sley_testkit::oracle_git(), repo, &["add", "one.txt", "keep.txt"]);
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-qm",
                    "base",
                ],
            );
        }

        for args in [
            vec!["update-index", "--again"],
            vec!["update-index", "-g"],
            vec!["update-index", "--again", "keep.txt"],
            vec!["update-index", "--remove", "--again"],
        ] {
            for repo in [&expected, &actual] {
                run_success(sley_testkit::oracle_git(), repo, &["reset", "-q", "--hard", "HEAD"]);
                fs::write(repo.join("one.txt"), b"staged-one").expect("stage one");
                fs::write(repo.join("keep.txt"), b"staged-keep").expect("stage keep");
                run_success(sley_testkit::oracle_git(), repo, &["add", "one.txt", "keep.txt"]);
                fs::write(repo.join("one.txt"), b"worktree-one").expect("modify one");
                fs::write(repo.join("keep.txt"), b"worktree-keep").expect("modify keep");
                if args == ["update-index", "--remove", "--again"] {
                    fs::remove_file(repo.join("keep.txt")).expect("remove keep");
                }
            }

            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
        }

        for repo in [&expected, &actual] {
            run_success(sley_testkit::oracle_git(), repo, &["reset", "-q", "--hard", "HEAD"]);
            fs::write(repo.join("keep.txt"), b"unstaged-only").expect("modify keep");
        }
        let args = ["update-index", "--again"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches(&expected, &actual, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_verbose_matches_upstream_git() {
    let root = unique_temp_dir("update-index-verbose");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--verbose", "keep.txt"],
            vec!["update-index", "--verbose", "--add", "new.txt"],
            vec!["update-index", "--verbose", "--remove", "one.txt"],
            vec!["update-index", "--verbose", "--force-remove", "keep.txt"],
            vec![
                "update-index",
                "--verbose",
                "--no-verbose",
                "--add",
                "new.txt",
            ],
            vec![
                "update-index",
                "--no-verbose",
                "--verbose",
                "--add",
                "new.txt",
            ],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_refresh_matches_upstream_git() {
    let root = unique_temp_dir("update-index-refresh");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--refresh"],
            vec!["update-index", "--refresh", "keep.txt"],
            vec!["update-index", "--really-refresh"],
            vec!["update-index", "-q", "--refresh"],
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
        }

        for args in [
            vec!["update-index", "--refresh"],
            vec!["update-index", "--refresh", "keep.txt"],
            vec!["update-index", "-q", "--refresh"],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }

        for args in [
            vec!["update-index", "--refresh"],
            vec!["update-index", "--really-refresh"],
            vec!["update-index", "-q", "--refresh"],
            vec!["update-index", "-q", "--really-refresh"],
            vec!["update-index", "--refresh", "keep.txt"],
            vec!["update-index", "--really-refresh", "keep.txt"],
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            run_success(
                sley_testkit::oracle_git(),
                &expected,
                &["update-index", "--assume-unchanged", "keep.txt"],
            );
            run_success(
                env!("CARGO_BIN_EXE_sley"),
                &actual,
                &["update-index", "--assume-unchanged", "keep.txt"],
            );
            fs::write(expected.join("keep.txt"), b"changed").expect("modify expected keep");
            fs::write(actual.join("keep.txt"), b"changed").expect("modify actual keep");
            fs::write(expected.join("one.txt"), b"changed").expect("modify expected one");
            fs::write(actual.join("one.txt"), b"changed").expect("modify actual one");
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
            assert_ls_files_verbose_matches(&expected, &actual, &format!("{args:?}"));
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_refresh_ignore_missing_matches_upstream_git() {
    let root = unique_temp_dir("update-index-refresh-ignore-missing");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--ignore-missing"],
            vec!["update-index", "--no-ignore-missing"],
            vec!["update-index", "--ignore-missing", "--refresh"],
            vec!["update-index", "--refresh", "--ignore-missing"],
            vec![
                "update-index",
                "--no-ignore-missing",
                "--ignore-missing",
                "--refresh",
            ],
            vec![
                "update-index",
                "--ignore-missing",
                "--no-ignore-missing",
                "--refresh",
            ],
            vec!["update-index", "-q", "--ignore-missing", "--refresh"],
            vec!["update-index", "--ignore-missing", "-q", "--refresh"],
            vec!["update-index", "--ignore-missing", "one.txt"],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_clear_resolve_undo_matches_upstream_git() {
    let root = unique_temp_dir("update-index-clear-resolve-undo");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--clear-resolve-undo"],
            vec!["update-index", "--clear-resolve-undo", "keep.txt"],
            vec!["update-index", "--clear-resolve-undo", "--add", "new.txt"],
            vec![
                "update-index",
                "--clear-resolve-undo",
                "--remove",
                "one.txt",
            ],
            vec![
                "update-index",
                "--clear-resolve-undo",
                "--show-index-version",
            ],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_assume_unchanged_matches_upstream_git() {
    let root = unique_temp_dir("update-index-assume-unchanged");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (label, args) in [
            (
                "assume",
                vec!["update-index", "--assume-unchanged", "keep.txt"],
            ),
            (
                "assume-then-clear",
                vec![
                    "update-index",
                    "--assume-unchanged",
                    "--no-assume-unchanged",
                    "keep.txt",
                ],
            ),
            (
                "clear-then-assume",
                vec![
                    "update-index",
                    "--no-assume-unchanged",
                    "--assume-unchanged",
                    "keep.txt",
                ],
            ),
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches_for_label(&expected, &actual, label);
            assert_ls_files_verbose_matches(&expected, &actual, label);
        }

        reset_clean_fixture(&expected);
        reset_clean_fixture(&actual);
        run_success(
            sley_testkit::oracle_git(),
            &expected,
            &["update-index", "--assume-unchanged", "keep.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual,
            &["update-index", "--assume-unchanged", "keep.txt"],
        );
        let args = ["update-index", "--no-assume-unchanged", "keep.txt"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches_for_label(&expected, &actual, "clear-existing-assume");
        assert_ls_files_verbose_matches(&expected, &actual, "clear-existing-assume");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_skip_worktree_matches_upstream_git() {
    let root = unique_temp_dir("update-index-skip-worktree");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (label, args) in [
            ("skip", vec!["update-index", "--skip-worktree", "keep.txt"]),
            (
                "skip-then-clear",
                vec![
                    "update-index",
                    "--skip-worktree",
                    "--no-skip-worktree",
                    "keep.txt",
                ],
            ),
            (
                "clear-then-skip",
                vec![
                    "update-index",
                    "--no-skip-worktree",
                    "--skip-worktree",
                    "keep.txt",
                ],
            ),
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches_for_label(&expected, &actual, label);
            assert_ls_files_verbose_matches(&expected, &actual, label);
            assert_index_version_matches(&expected, &actual, label);
        }

        reset_clean_fixture(&expected);
        reset_clean_fixture(&actual);
        run_success(
            sley_testkit::oracle_git(),
            &expected,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        let args = ["update-index", "--no-skip-worktree", "keep.txt"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches_for_label(&expected, &actual, "clear-existing-skip");
        assert_ls_files_verbose_matches(&expected, &actual, "clear-existing-skip");
        assert_index_version_matches(&expected, &actual, "clear-existing-skip");

        reset_clean_fixture(&expected);
        reset_clean_fixture(&actual);
        run_success(
            sley_testkit::oracle_git(),
            &expected,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        let args = ["update-index", "--no-skip-worktree", "keep.txt"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches_for_label(&expected, &actual, "clear-upstream-written-skip");
        assert_ls_files_verbose_matches(&expected, &actual, "clear-upstream-written-skip");
        assert_index_version_matches(&expected, &actual, "clear-upstream-written-skip");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_ignore_skip_worktree_entries_matches_upstream_git() {
    let root = unique_temp_dir("update-index-ignore-skip-worktree");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (label, args) in [
            ("path-update", vec!["update-index", "keep.txt"]),
            (
                "ignore-path-update",
                vec!["update-index", "--ignore-skip-worktree-entries", "keep.txt"],
            ),
            ("remove", vec!["update-index", "--remove", "keep.txt"]),
            (
                "ignore-remove",
                vec![
                    "update-index",
                    "--ignore-skip-worktree-entries",
                    "--remove",
                    "keep.txt",
                ],
            ),
            (
                "ignore-then-no-ignore-remove",
                vec![
                    "update-index",
                    "--ignore-skip-worktree-entries",
                    "--no-ignore-skip-worktree-entries",
                    "--remove",
                    "keep.txt",
                ],
            ),
            (
                "no-ignore-then-ignore-remove",
                vec![
                    "update-index",
                    "--no-ignore-skip-worktree-entries",
                    "--ignore-skip-worktree-entries",
                    "--remove",
                    "keep.txt",
                ],
            ),
            (
                "ignore-force-remove",
                vec![
                    "update-index",
                    "--ignore-skip-worktree-entries",
                    "--force-remove",
                    "keep.txt",
                ],
            ),
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            run_success(
                sley_testkit::oracle_git(),
                &expected,
                &["update-index", "--skip-worktree", "keep.txt"],
            );
            run_success(
                sley_testkit::oracle_git(),
                &actual,
                &["update-index", "--skip-worktree", "keep.txt"],
            );
            fs::write(expected.join("keep.txt"), b"changed").expect("modify expected keep");
            fs::write(actual.join("keep.txt"), b"changed").expect("modify actual keep");
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches_for_label(&expected, &actual, label);
            assert_ls_files_verbose_matches(&expected, &actual, label);
            assert_index_version_matches(&expected, &actual, label);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_fsmonitor_valid_matches_upstream_git() {
    let root = unique_temp_dir("update-index-fsmonitor-valid");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--fsmonitor-valid"],
            vec!["update-index", "--no-fsmonitor-valid"],
            vec!["update-index", "--fsmonitor-valid", "keep.txt"],
            vec!["update-index", "--no-fsmonitor-valid", "keep.txt"],
            vec![
                "update-index",
                "--fsmonitor-valid",
                "--no-fsmonitor-valid",
                "keep.txt",
            ],
            vec![
                "update-index",
                "--no-fsmonitor-valid",
                "--fsmonitor-valid",
                "keep.txt",
            ],
            vec!["update-index", "--fsmonitor-valid", "--remove", "one.txt"],
            vec!["update-index", "--fsmonitor-valid", "--add", "new.txt"],
            vec!["update-index", "--fsmonitor-valid", "missing.txt"],
            vec!["update-index", "--fsmonitor-valid", "--show-index-version"],
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
                assert_ls_files_verbose_matches(&expected, &actual, &format!("{args:?}"));
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_info_only_matches_upstream_git() {
    let root = unique_temp_dir("update-index-info-only");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (label, args) in [
            (
                "info-only-path",
                vec!["update-index", "--add", "--info-only", "new.txt"],
            ),
            (
                "no-info-only-path",
                vec![
                    "update-index",
                    "--add",
                    "--info-only",
                    "--no-info-only",
                    "new.txt",
                ],
            ),
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches_for_label(&expected, &actual, label);
            assert_object_existence_matches(&expected, &actual, "new.txt", label);
        }

        let args = ["update-index", "--add", "--info-only", "--stdin"];
        let stdin = b"new.txt\n";
        reset_fixture(&expected);
        reset_fixture(&actual);
        let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &args, stdin);
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, stdin);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches_for_label(&expected, &actual, "info-only-stdin");
        assert_object_existence_matches(&expected, &actual, "new.txt", "info-only-stdin");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_show_index_version_matches_upstream_git() {
    let root = unique_temp_dir("update-index-show-index-version");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for args in [
            vec!["update-index", "--show-index-version"],
            vec![
                "update-index",
                "--show-index-version",
                "--no-show-index-version",
            ],
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
        }

        reset_fixture(&expected);
        reset_fixture(&actual);

        for args in [
            vec!["update-index", "--show-index-version"],
            vec!["update-index", "--show-index-version", "keep.txt"],
            vec![
                "update-index",
                "--show-index-version",
                "--no-show-index-version",
            ],
            vec![
                "update-index",
                "--no-show-index-version",
                "--show-index-version",
            ],
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches(&expected, &actual, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_index_version_matches_upstream_git() {
    let root = unique_temp_dir("update-index-index-version");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (label, args) in [
            ("empty-split", vec!["update-index", "--index-version", "2"]),
            ("empty-equals", vec!["update-index", "--index-version=2"]),
            (
                "empty-v3-split",
                vec!["update-index", "--index-version", "3"],
            ),
            ("empty-v3-equals", vec!["update-index", "--index-version=3"]),
            (
                "empty-v4-split",
                vec!["update-index", "--index-version", "4"],
            ),
            ("empty-v4-equals", vec!["update-index", "--index-version=4"]),
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_version_matches(&expected, &actual, label);
            assert_index_matches_for_label(&expected, &actual, label);
        }

        for (label, args) in [
            (
                "populated-split",
                vec!["update-index", "--index-version", "2"],
            ),
            (
                "populated-equals",
                vec!["update-index", "--index-version=2"],
            ),
            (
                "populated-v3-split",
                vec!["update-index", "--index-version", "3"],
            ),
            (
                "populated-v3-equals",
                vec!["update-index", "--index-version=3"],
            ),
            (
                "populated-v4-split",
                vec!["update-index", "--index-version", "4"],
            ),
            (
                "populated-v4-equals",
                vec!["update-index", "--index-version=4"],
            ),
        ] {
            reset_clean_fixture(&expected);
            reset_clean_fixture(&actual);
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_version_matches(&expected, &actual, label);
            assert_index_matches_for_label(&expected, &actual, label);
        }

        reset_clean_fixture(&expected);
        reset_clean_fixture(&actual);
        run_success(
            sley_testkit::oracle_git(),
            &expected,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["update-index", "--skip-worktree", "keep.txt"],
        );
        for (label, args) in [
            (
                "extended-v2-request",
                vec!["update-index", "--index-version", "2"],
            ),
            (
                "extended-v3-request",
                vec!["update-index", "--index-version", "3"],
            ),
            (
                "extended-v4-request",
                vec!["update-index", "--index-version", "4"],
            ),
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_version_matches(&expected, &actual, label);
            assert_index_matches_for_label(&expected, &actual, label);
            assert_ls_files_verbose_matches(&expected, &actual, label);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_cacheinfo_matches_upstream_git() {
    let root = unique_temp_dir("update-index-cacheinfo");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);
        reset_fixture(&expected);
        reset_fixture(&actual);
        let blob = String::from_utf8(
            run_with_stdin(
                sley_testkit::oracle_git(),
                &expected,
                &["hash-object", "-w", "--stdin"],
                b"cacheinfo data",
            )
            .stdout,
        )
        .expect("blob oid utf8")
        .trim()
        .to_string();

        for (label, args) in [
            (
                "tuple",
                vec![
                    "update-index".to_string(),
                    "--add".to_string(),
                    "--cacheinfo".to_string(),
                    format!("100644,{blob},path/to/file"),
                ],
            ),
            (
                "split",
                vec![
                    "update-index".to_string(),
                    "--add".to_string(),
                    "--cacheinfo".to_string(),
                    "100755".to_string(),
                    blob.clone(),
                    "exe".to_string(),
                ],
            ),
            (
                "existing-without-add",
                vec![
                    "update-index".to_string(),
                    "--cacheinfo".to_string(),
                    format!("100755,{blob},keep.txt"),
                ],
            ),
            (
                "missing-without-add",
                vec![
                    "update-index".to_string(),
                    "--cacheinfo".to_string(),
                    format!("100644,{blob},missingadd"),
                ],
            ),
        ] {
            let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
            let expected_output = run(sley_testkit::oracle_git(), &expected, &arg_refs);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &arg_refs);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &arg_refs);
            if expected_success {
                assert_index_matches_for_label(&expected, &actual, label);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_index_info_matches_upstream_git() {
    let root = unique_temp_dir("update-index-index-info");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);
        let one = String::from_utf8(
            run_with_stdin(sley_testkit::oracle_git(), &expected, &["hash-object", "-w", "--stdin"], b"one").stdout,
        )
        .expect("first blob oid utf8")
        .trim()
        .to_string();
        let two = String::from_utf8(
            run_with_stdin(sley_testkit::oracle_git(), &expected, &["hash-object", "-w", "--stdin"], b"two").stdout,
        )
        .expect("second blob oid utf8")
        .trim()
        .to_string();

        let simple = format!("100644 {one}\tpath/to/file\n");
        let remove = "0 0000000000000000000000000000000000000000\tpath/to/file\n".to_string();
        let staged = format!(
            "0 0000000000000000000000000000000000000000\tconflict\n100644 {one} 1\tconflict\n100644 {two} 2\tconflict\n"
        );

        for (label, stdin) in [
            ("simple", simple.as_bytes()),
            ("remove", remove.as_bytes()),
            ("staged", staged.as_bytes()),
        ] {
            let args = ["update-index", "--index-info"];
            let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &args, stdin);
            let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, stdin);
            assert_same_output(actual_output, expected_output, &args);
            assert_index_matches_for_label(&expected, &actual, label);
        }

        let args = ["update-index", "--index-info", "extra"];
        let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &args, b"");
        let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, b"");
        assert_same_output(actual_output, expected_output, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_stdin_modes_match_upstream_git() {
    let root = unique_temp_dir("update-index-stdin-modes");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q"]);

        for (args, stdin) in [
            (
                vec!["update-index", "--add", "--stdin"],
                b"new.txt\nz.txt\n".to_vec(),
            ),
            (
                vec!["update-index", "--add", "-z", "--stdin"],
                b"new.txt\0z.txt\0".to_vec(),
            ),
            (
                vec!["update-index", "--add", "z.txt", "--stdin"],
                b"new.txt\n".to_vec(),
            ),
            (vec!["update-index", "--stdin"], Vec::new()),
            (
                vec!["update-index", "--add", "--stdin", "z.txt"],
                b"new.txt\n".to_vec(),
            ),
        ] {
            reset_fixture(&expected);
            reset_fixture(&actual);
            let expected_output = run_with_stdin(sley_testkit::oracle_git(), &expected, &args, &stdin);
            let actual_output = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, &stdin);
            let expected_success = expected_output.status.success();
            assert_same_output(actual_output, expected_output, &args);
            if expected_success {
                assert_index_matches(&expected, &actual, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn update_index_adds_sha256_index_entries() {
    let root = unique_temp_dir("update-index-sha256");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success(sley_testkit::oracle_git(), &expected, &["init", "-q", "--object-format=sha256"]);
        run_success(sley_testkit::oracle_git(), &actual, &["init", "-q", "--object-format=sha256"]);
        fs::write(expected.join("a.txt"), b"sha256\n").expect("write expected fixture");
        fs::write(actual.join("a.txt"), b"sha256\n").expect("write actual fixture");

        let args = ["update-index", "--add", "a.txt"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_index_matches(&expected, &actual, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
