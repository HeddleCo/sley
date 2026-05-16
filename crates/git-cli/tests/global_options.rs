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

fn prepare_fixed_commit_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo dir");
    let init = run("git", root, &["init", "-q"]);
    assert!(init.status.success(), "git init failed");
    fs::write(
        root.join("commit.txt"),
        b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@b> 0 +0000\ncommitter A <a@b> 0 +0000\n\nbase\n",
    )
    .expect("write fixed commit");
    let oid_output = run(
        "git",
        root,
        &["hash-object", "-w", "-t", "commit", "commit.txt"],
    );
    assert!(oid_output.status.success(), "git hash-object commit failed");
    let oid = String::from_utf8(oid_output.stdout).expect("commit oid utf8");
    let update = run("git", root, &["update-ref", "HEAD", oid.trim()]);
    assert!(update.status.success(), "git update-ref HEAD failed");
}

#[test]
fn global_version_and_noop_flags_match_upstream_git() {
    let root = unique_temp_dir("global-version-noop");
    let repo = root.join("repo");
    let result = (|| {
        fs::create_dir_all(&repo).expect("create repo dir");
        prepare_fixed_commit_repo(&repo);

        for args in [
            vec!["version"],
            vec!["--version"],
            vec!["-v"],
            vec!["--no-pager", "--version"],
            vec!["-P", "--version"],
            vec!["--no-optional-locks", "--version"],
            vec!["--no-advice", "--version"],
            vec!["--no-replace-objects", "--version"],
            vec!["--no-lazy-fetch", "--version"],
            vec![
                "--no-pager",
                "--no-optional-locks",
                "rev-parse",
                "--show-prefix",
            ],
        ] {
            let expected_output = run("git", &repo, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &repo, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_git_dir_and_work_tree_match_upstream_git_for_rev_parse() {
    let root = unique_temp_dir("global-git-dir-work-tree");
    let repo = root.join("repo");
    let sub = repo.join("sub");
    let result = (|| {
        prepare_fixed_commit_repo(&repo);
        fs::create_dir_all(&sub).expect("create subdir");

        for (cwd, args) in [
            (&repo, vec!["--git-dir=.git", "rev-parse", "--git-dir"]),
            (
                &repo,
                vec!["--git-dir", ".git", "rev-parse", "--verify", "HEAD"],
            ),
            (&sub, vec!["--git-dir=../.git", "rev-parse", "--git-dir"]),
            (
                &sub,
                vec!["--git-dir=../.git", "rev-parse", "--show-toplevel"],
            ),
            (
                &sub,
                vec![
                    "--git-dir=../.git",
                    "--work-tree=..",
                    "rev-parse",
                    "--show-prefix",
                ],
            ),
            (&sub, vec!["--work-tree=..", "rev-parse", "--show-prefix"]),
        ] {
            let expected_output = run("git", cwd, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn repository_environment_matches_upstream_git_for_rev_parse() {
    let root = unique_temp_dir("repository-env");
    let repo = root.join("repo");
    let sub = repo.join("sub");
    let result = (|| {
        prepare_fixed_commit_repo(&repo);
        fs::create_dir_all(&sub).expect("create subdir");

        for (cwd, args, envs) in [
            (
                &repo,
                vec!["rev-parse", "--git-dir"],
                vec![("GIT_DIR", ".git")],
            ),
            (
                &repo,
                vec!["rev-parse", "--show-toplevel"],
                vec![("GIT_DIR", ".git")],
            ),
            (
                &sub,
                vec!["rev-parse", "--git-dir"],
                vec![("GIT_DIR", "../.git")],
            ),
            (
                &sub,
                vec!["rev-parse", "--show-toplevel"],
                vec![("GIT_DIR", "../.git")],
            ),
            (
                &sub,
                vec!["rev-parse", "--show-prefix"],
                vec![("GIT_DIR", "../.git"), ("GIT_WORK_TREE", "..")],
            ),
            (
                &sub,
                vec!["rev-parse", "--show-toplevel"],
                vec![("GIT_DIR", "../.git"), ("GIT_WORK_TREE", "..")],
            ),
            (
                &sub,
                vec!["rev-parse", "--show-prefix"],
                vec![("GIT_WORK_TREE", "..")],
            ),
            (
                &sub,
                vec!["rev-parse", "--show-toplevel"],
                vec![("GIT_WORK_TREE", "..")],
            ),
        ] {
            let expected_output = run_with_env("git", cwd, &args, &envs);
            let actual_output = run_with_env(env!("CARGO_BIN_EXE_git-rs"), cwd, &args, &envs);
            assert_same_output(actual_output, expected_output, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_bare_matches_upstream_git_for_rev_parse() {
    let root = unique_temp_dir("global-bare");
    let bare = root.join("bare.git");
    let result = (|| {
        fs::create_dir_all(&root).expect("create root dir");
        let init = run(
            "git",
            &root,
            &[
                "init",
                "--bare",
                "-q",
                bare.to_str().expect("bare path utf8"),
            ],
        );
        assert!(init.status.success(), "git init --bare failed");

        for args in [
            vec!["--bare", "rev-parse", "--git-dir"],
            vec!["--bare", "rev-parse", "--is-bare-repository"],
            vec!["--bare", "rev-parse", "--is-inside-git-dir"],
            vec!["--bare", "rev-parse", "--is-inside-work-tree"],
            vec!["--bare", "rev-parse", "--show-toplevel"],
            vec![
                "--bare",
                "--work-tree=.",
                "rev-parse",
                "--is-bare-repository",
            ],
            vec!["--bare", "--work-tree=.", "rev-parse", "--show-toplevel"],
        ] {
            let expected_output = run("git", &bare, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &bare, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_c_option_matches_upstream_git() {
    let root = unique_temp_dir("global-c");
    let repo = root.join("repo");
    let result = (|| {
        fs::create_dir_all(repo.join("sub")).expect("create repo dirs");
        let expected_init = run("git", &root, &["-C", "repo", "init", "-q"]);
        let actual_init = run(
            env!("CARGO_BIN_EXE_git-rs"),
            &root,
            &["-C", "repo", "init", "-q"],
        );
        assert_same_output(actual_init, expected_init, &["-C", "repo", "init", "-q"]);

        let args = ["-C", "repo", "-C", "sub", "rev-parse", "--show-prefix"];
        let expected_output = run("git", &root, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual_output, expected_output, &args);

        let args = ["-C"];
        let expected_output = run("git", &root, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual_output, expected_output, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_config_core_abbrev_matches_upstream_git_for_rev_parse_short() {
    let root = unique_temp_dir("global-config-abbrev");
    let expected = root.join("expected");
    let actual = root.join("actual");
    let result = (|| {
        prepare_fixed_commit_repo(&expected);
        prepare_fixed_commit_repo(&actual);

        for args in [
            vec!["rev-parse", "--short", "HEAD"],
            vec!["-c", "core.abbrev=12", "rev-parse", "--short", "HEAD"],
            vec!["-c", "core.abbrev=4", "rev-parse", "--short", "HEAD"],
            vec!["-c", "core.abbrev=no", "rev-parse", "--short", "HEAD"],
        ] {
            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }

        for args in [
            vec![
                "--config-env=core.abbrev=GIT_RS_TEST_ABBREV",
                "rev-parse",
                "--short",
                "HEAD",
            ],
            vec![
                "--config-env",
                "core.abbrev=GIT_RS_TEST_ABBREV",
                "rev-parse",
                "--short",
                "HEAD",
            ],
            vec![
                "--config-env=core.abbrev=GIT_RS_TEST_ABBREV",
                "-c",
                "core.abbrev=4",
                "rev-parse",
                "--short",
                "HEAD",
            ],
        ] {
            let expected_output =
                run_with_env("git", &expected, &args, &[("GIT_RS_TEST_ABBREV", "12")]);
            let actual_output = run_with_env(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual,
                &args,
                &[("GIT_RS_TEST_ABBREV", "12")],
            );
            assert_same_output(actual_output, expected_output, &args);
        }

        for (args, envs) in [
            (
                vec!["rev-parse", "--short", "HEAD"],
                vec![
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "core.abbrev"),
                    ("GIT_CONFIG_VALUE_0", "12"),
                ],
            ),
            (
                vec!["rev-parse", "--short", "HEAD"],
                vec![
                    ("GIT_CONFIG_COUNT", "2"),
                    ("GIT_CONFIG_KEY_0", "core.abbrev"),
                    ("GIT_CONFIG_VALUE_0", "12"),
                    ("GIT_CONFIG_KEY_1", "core.abbrev"),
                    ("GIT_CONFIG_VALUE_1", "4"),
                ],
            ),
            (
                vec!["-c", "core.abbrev=4", "rev-parse", "--short", "HEAD"],
                vec![
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "core.abbrev"),
                    ("GIT_CONFIG_VALUE_0", "12"),
                ],
            ),
            (
                vec!["rev-parse", "--short", "HEAD"],
                vec![("GIT_CONFIG_COUNT", "bad")],
            ),
            (
                vec!["rev-parse", "--short", "HEAD"],
                vec![
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "core.abbrev"),
                ],
            ),
            (
                vec!["rev-parse", "--short", "HEAD"],
                vec![("GIT_CONFIG_COUNT", "-1")],
            ),
        ] {
            let expected_output = run_with_env("git", &expected, &args, &envs);
            let actual_output = run_with_env(env!("CARGO_BIN_EXE_git-rs"), &actual, &args, &envs);
            assert_same_output(actual_output, expected_output, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_config_log_all_ref_updates_matches_upstream_git_for_update_ref() {
    let root = unique_temp_dir("global-config-logall");
    let expected = root.join("expected");
    let actual = root.join("actual");
    let result = (|| {
        prepare_fixed_commit_repo(&expected);
        prepare_fixed_commit_repo(&actual);
        let oid = String::from_utf8(run("git", &expected, &["rev-parse", "HEAD"]).stdout)
            .expect("HEAD oid utf8");
        let oid = oid.trim();

        let expected_args = [
            "-c",
            "core.logAllRefUpdates=false",
            "update-ref",
            "refs/heads/global-false",
            oid,
        ];
        let actual_args = [
            "-c",
            "core.logAllRefUpdates=false",
            "update-ref",
            "refs/heads/global-false",
            oid,
        ];
        let expected_output = run("git", &expected, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &actual, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(
            actual.join(".git/logs/refs/heads/global-false").exists(),
            expected.join(".git/logs/refs/heads/global-false").exists()
        );

        let expected_args = [
            "-c",
            "core.logAllRefUpdates=always",
            "update-ref",
            "refs/tags/global-always",
            oid,
        ];
        let actual_args = [
            "-c",
            "core.logAllRefUpdates=always",
            "update-ref",
            "refs/tags/global-always",
            oid,
        ];
        let expected_output = run("git", &expected, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &actual, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(
            actual.join(".git/logs/refs/tags/global-always").exists(),
            expected.join(".git/logs/refs/tags/global-always").exists()
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn global_config_init_default_branch_matches_upstream_git() {
    let root = unique_temp_dir("global-config-init");
    let expected = root.join("expected");
    let actual = root.join("actual");
    let expected_last = root.join("expected-last");
    let actual_last = root.join("actual-last");
    let expected_cli = root.join("expected-cli");
    let actual_cli = root.join("actual-cli");
    let result = (|| {
        fs::create_dir_all(&root).expect("create root dir");
        let args = [
            "-c",
            "init.defaultBranch=trunk",
            "init",
            "-q",
            expected.to_str().expect("expected path utf8"),
        ];
        let expected_output = run("git", &root, &args);
        let args = [
            "-c",
            "init.defaultBranch=trunk",
            "init",
            "-q",
            actual.to_str().expect("actual path utf8"),
        ];
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual_output, expected_output, &args);
        assert_eq!(
            fs::read(actual.join(".git/HEAD")).expect("read actual HEAD"),
            fs::read(expected.join(".git/HEAD")).expect("read expected HEAD")
        );

        let expected_args = [
            "-c",
            "init.defaultBranch=trunk",
            "-c",
            "init.defaultBranch=final",
            "init",
            "-q",
            expected_last.to_str().expect("expected-last path utf8"),
        ];
        let actual_args = [
            "-c",
            "init.defaultBranch=trunk",
            "-c",
            "init.defaultBranch=final",
            "init",
            "-q",
            actual_last.to_str().expect("actual-last path utf8"),
        ];
        let expected_output = run("git", &root, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &root, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(
            fs::read(actual_last.join(".git/HEAD")).expect("read actual-last HEAD"),
            fs::read(expected_last.join(".git/HEAD")).expect("read expected-last HEAD")
        );

        let expected_args = [
            "-c",
            "init.defaultBranch=trunk",
            "init",
            "-q",
            "-b",
            "cli",
            expected_cli.to_str().expect("expected-cli path utf8"),
        ];
        let actual_args = [
            "-c",
            "init.defaultBranch=trunk",
            "init",
            "-q",
            "-b",
            "cli",
            actual_cli.to_str().expect("actual-cli path utf8"),
        ];
        let expected_output = run("git", &root, &expected_args);
        let actual_output = run(env!("CARGO_BIN_EXE_git-rs"), &root, &actual_args);
        assert_same_output(actual_output, expected_output, &expected_args);
        assert_eq!(
            fs::read(actual_cli.join(".git/HEAD")).expect("read actual-cli HEAD"),
            fs::read(expected_cli.join(".git/HEAD")).expect("read expected-cli HEAD")
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
