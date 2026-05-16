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

fn embed_submodule_git_dir(superproject: &Path, path: &str) {
    let submodule = superproject.join(path);
    let dot_git = submodule.join(".git");
    if dot_git.is_dir() {
        return;
    }
    let contents = fs::read_to_string(&dot_git).expect("read submodule gitfile");
    let target = contents
        .trim()
        .strip_prefix("gitdir:")
        .expect("submodule gitfile has gitdir")
        .trim();
    let target = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        submodule.join(target)
    };
    let target = fs::canonicalize(target).expect("canonicalize submodule gitdir");
    fs::remove_file(&dot_git).expect("remove submodule gitfile");
    fs::rename(target, dot_git).expect("embed submodule gitdir");
}

#[test]
fn submodule_status_matches_upstream_git() {
    let root = unique_temp_dir("submodule-status");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child",
                "sub",
            ],
        );
        let nested = superproject.join("nested");
        fs::create_dir_all(&nested).expect("create nested superproject dir");

        for args in [
            vec!["submodule"],
            vec!["submodule", "status"],
            vec!["submodule", "status", "--cached"],
            vec!["submodule", "status", "--quiet"],
            vec!["submodule", "status", "-q"],
            vec!["submodule", "status", "--recursive"],
            vec!["submodule", "status", "sub"],
            vec!["submodule", "status", "./sub"],
            vec!["submodule", "status", "sub/"],
            vec!["submodule", "status", "--", "sub"],
            vec!["submodule", "--quiet", "status"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
        for args in [
            vec!["submodule", "status"],
            vec!["submodule", "status", "../sub"],
            vec!["submodule", "status", "sub"],
        ] {
            let expected = run("git", &nested, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &nested, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "add submodule",
            ],
        );
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child update",
            ],
        );
        run_success("git", &superproject.join("sub"), &["fetch", "-q", "origin"]);
        run_success(
            "git",
            &superproject.join("sub"),
            &["checkout", "-q", "FETCH_HEAD"],
        );

        for args in [
            vec!["submodule", "status"],
            vec!["submodule", "status", "--cached"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            "git",
            &superproject,
            &["submodule", "deinit", "-q", "-f", "sub"],
        );
        for args in [
            vec!["submodule", "status"],
            vec!["submodule", "status", "--cached"],
            vec!["submodule", "status", "sub"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child update",
            ],
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_status_sorts_multiple_submodules_like_upstream_git() {
    let root = unique_temp_dir("submodule-status-sort");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child_a = root.join("child-a");
        let child_b = root.join("child-b");
        let superproject = root.join("super");
        fs::create_dir_all(&child_a).expect("create child a repo");
        fs::create_dir_all(&child_b).expect("create child b repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        for child in [&child_a, &child_b] {
            run_success("git", child, &["init", "-q"]);
            run_success(
                "git",
                child,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "child",
                ],
            );
        }
        run_success("git", &superproject, &["init", "-q"]);
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child-b",
                "zed",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child-a",
                "alpha",
            ],
        );
        let nested = superproject.join("nested");
        fs::create_dir_all(&nested).expect("create nested superproject dir");

        for args in [
            vec!["submodule", "status"],
            vec!["submodule", "status", "zed", "alpha"],
            vec!["submodule", "status", "alpha", "zed"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
        let args = ["submodule", "status", "../zed", "../alpha"];
        let expected = run("git", &nested, &args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &nested, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_status_prefers_exact_tag_suffix_like_upstream_git() {
    let root = unique_temp_dir("submodule-status-tag-suffix");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "first",
            ],
        );
        run_success("git", &child, &["tag", "v1"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "second",
            ],
        );
        run_success("git", &child, &["tag", "v2"]);
        run_success("git", &superproject, &["init", "-q"]);
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child",
                "sub",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "add submodule",
            ],
        );

        let args = ["submodule", "status"];
        let expected = run("git", &superproject, &args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        assert_same_output(actual, expected, &args);

        run_success("git", &superproject.join("sub"), &["checkout", "-q", "v1"]);
        let expected = run("git", &superproject, &args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_status_directory_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("submodule-status-directory-pathspecs");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child_a = root.join("child-a");
        let child_b = root.join("child-b");
        let superproject = root.join("super");
        fs::create_dir_all(&child_a).expect("create child a repo");
        fs::create_dir_all(&child_b).expect("create child b repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        for child in [&child_a, &child_b] {
            run_success("git", child, &["init", "-q"]);
            run_success(
                "git",
                child,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "child",
                ],
            );
        }
        run_success("git", &superproject, &["init", "-q"]);
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child-a",
                "deps/a",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child-b",
                "deps/nested/b",
            ],
        );
        let nested = superproject.join("nested");
        fs::create_dir_all(&nested).expect("create nested superproject dir");

        for args in [
            vec!["submodule", "status", "."],
            vec!["submodule", "status", "deps"],
            vec!["submodule", "status", "deps/"],
            vec!["submodule", "status", "deps/nested"],
            vec!["submodule", "status", "deps", "deps/a"],
            vec!["submodule", "status", "deps/other"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
        for args in [
            vec!["submodule", "status", "../deps"],
            vec!["submodule", "status", "."],
        ] {
            let expected = run("git", &nested, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &nested, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_status_recursive_matches_upstream_git() {
    let root = unique_temp_dir("submodule-status-recursive");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let grandchild = root.join("grandchild");
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&grandchild).expect("create grandchild repo");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &grandchild, &["init", "-q"]);
        run_success(
            "git",
            &grandchild,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "grandchild",
            ],
        );
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../grandchild",
                "nested/grandchild",
            ],
        );
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "add nested submodule",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                "../child",
                "deps/child",
            ],
        );

        for args in [
            vec!["submodule", "status", "--recursive"],
            vec!["submodule", "status", "--recursive", "deps/child"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }

        for args in [
            vec!["submodule", "status", "--no-recursive"],
            vec!["submodule", "status", "--recursive", "--no-recursive"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_init_registers_local_config_like_upstream_git() {
    let root = unique_temp_dir("submodule-init");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.update",
                "checkout",
            ],
        );
        run_success(
            "git",
            &superproject,
            &["config", "--remove-section", "submodule.deps/child"],
        );

        let args = ["submodule", "init"];
        let expected = run("git", &superproject, &args);
        let expected_config = run_success(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        run_success(
            "git",
            &superproject,
            &["config", "--remove-section", "submodule.deps/child"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_config = run_success(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(actual_config, expected_config);

        run_success(
            "git",
            &superproject,
            &["config", "--remove-section", "submodule.deps/child"],
        );
        let quiet_args = ["submodule", "init", "--quiet"];
        let expected = run("git", &superproject, &quiet_args);
        run_success(
            "git",
            &superproject,
            &["config", "--remove-section", "submodule.deps/child"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &quiet_args);
        assert_same_output(actual, expected, &quiet_args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_sync_updates_registered_urls_like_upstream_git() {
    let root = unique_temp_dir("submodule-sync");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child_a = root.join("child-a");
        let child_b = root.join("child-b");
        let superproject = root.join("super");
        fs::create_dir_all(&child_a).expect("create child a repo");
        fs::create_dir_all(&child_b).expect("create child b repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        for child in [&child_a, &child_b] {
            run_success("git", child, &["init", "-q"]);
            run_success(
                "git",
                child,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "child",
                ],
            );
        }
        run_success("git", &superproject, &["init", "-q"]);
        let child_a_url = child_a.to_string_lossy().into_owned();
        let child_b_url = child_b.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_a_url,
                "deps/a",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_b_url,
                "deps/b",
            ],
        );
        for name in ["deps/a", "deps/b"] {
            run_success(
                "git",
                &superproject,
                &["config", &format!("submodule.{name}.url"), "old-url"],
            );
        }

        let args = ["submodule", "sync", "deps/a"];
        let expected = run("git", &superproject, &args);
        let expected_config = run_success(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        run_success(
            "git",
            &superproject,
            &["config", "submodule.deps/a.url", "old-url"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_config = run_success(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(actual_config, expected_config);

        run_success(
            "git",
            &superproject,
            &["config", "submodule.deps/a.url", "old-url"],
        );
        let quiet_args = ["submodule", "sync", "--quiet", "--recursive", "deps/a"];
        let expected = run("git", &superproject, &quiet_args);
        run_success(
            "git",
            &superproject,
            &["config", "submodule.deps/a.url", "old-url"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &quiet_args);
        assert_same_output(actual, expected, &quiet_args);

        let bad_args = ["submodule", "sync", "--no-recursive"];
        let expected = run("git", &superproject, &bad_args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &bad_args);
        assert_same_output(actual, expected, &bad_args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_set_url_updates_gitmodules_and_local_config_like_upstream_git() {
    let root = unique_temp_dir("submodule-set-url");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );

        let args = ["submodule", "set-url", "deps/child", "new-url"];
        let expected = run("git", &superproject, &args);
        let expected_gitmodules = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.url",
            ],
        );
        let expected_local = run_success(
            "git",
            &superproject,
            &["config", "--local", "submodule.deps/child.url"],
        );
        run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.url",
                &child_url,
            ],
        );
        run_success(
            "git",
            &superproject,
            &["config", "--local", "submodule.deps/child.url", &child_url],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_gitmodules = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.url",
            ],
        );
        let actual_local = run_success(
            "git",
            &superproject,
            &["config", "--local", "submodule.deps/child.url"],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(actual_gitmodules, expected_gitmodules);
        assert_eq!(actual_local, expected_local);

        let quiet_args = ["submodule", "set-url", "--quiet", "deps/child", "quiet-url"];
        let expected = run("git", &superproject, &quiet_args);
        run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.url",
                "new-url",
            ],
        );
        run_success(
            "git",
            &superproject,
            &["config", "--local", "submodule.deps/child.url", "new-url"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &quiet_args);
        assert_same_output(actual, expected, &quiet_args);

        let missing_args = ["submodule", "set-url", "missing", "new-url"];
        let expected = run("git", &superproject, &missing_args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &missing_args);
        assert_same_output(actual, expected, &missing_args);

        let usage_args = ["submodule", "set-url"];
        let expected = run("git", &superproject, &usage_args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &usage_args);
        assert_same_output(actual, expected, &usage_args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_set_branch_updates_gitmodules_like_upstream_git() {
    let root = unique_temp_dir("submodule-set-branch");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );

        let args = ["submodule", "set-branch", "--branch", "main", "deps/child"];
        let expected = run("git", &superproject, &args);
        let expected_branch = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.branch",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "--unset",
                "submodule.deps/child.branch",
            ],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_branch = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.branch",
            ],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(actual_branch, expected_branch);

        let default_args = ["submodule", "set-branch", "--default", "deps/child"];
        let expected = run("git", &superproject, &default_args);
        let expected_branch = run(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "--get",
                "submodule.deps/child.branch",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "submodule.deps/child.branch",
                "main",
            ],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &default_args);
        let actual_branch = run(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".gitmodules",
                "--get",
                "submodule.deps/child.branch",
            ],
        );
        assert_same_output(actual, expected, &default_args);
        assert_same_output(actual_branch, expected_branch, &["config", "--get"]);

        for args in [
            vec!["submodule", "set-branch"],
            vec!["submodule", "set-branch", "--branch", "main"],
            vec![
                "submodule",
                "set-branch",
                "--branch",
                "main",
                "--default",
                "deps/child",
            ],
            vec!["submodule", "set-branch", "--branch", "main", "missing"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_foreach_runs_commands_like_upstream_git() {
    let root = unique_temp_dir("submodule-foreach");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );

        for args in [
            vec![
                "submodule",
                "foreach",
                "printf 'name=%s sm=%s display=%s sha=%s top=%s\\n' \"$name\" \"$sm_path\" \"$displaypath\" \"$sha1\" \"$toplevel\"",
            ],
            vec!["submodule", "foreach", "--quiet", "pwd"],
            vec!["submodule", "--quiet", "foreach", "pwd"],
            vec!["submodule", "foreach"],
            vec!["submodule", "foreach", "false"],
            vec!["submodule", "foreach", "--no-recursive", "pwd"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_foreach_recursive_matches_upstream_git() {
    let root = unique_temp_dir("submodule-foreach-recursive");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let grandchild = root.join("grandchild");
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&grandchild).expect("create grandchild repo");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &grandchild, &["init", "-q"]);
        run_success(
            "git",
            &grandchild,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "grandchild",
            ],
        );
        run_success("git", &child, &["init", "-q"]);
        let grandchild_url = grandchild.to_string_lossy().into_owned();
        run_success(
            "git",
            &child,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &grandchild_url,
                "nested/grandchild",
            ],
        );
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );
        run_success(
            "git",
            &superproject.join("deps/child"),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "-q",
                "--recursive",
            ],
        );

        let args = [
            "submodule",
            "foreach",
            "--recursive",
            "printf 'name=%s sm=%s display=%s top=%s pwd=%s\\n' \"$name\" \"$sm_path\" \"$displaypath\" \"$toplevel\" \"$PWD\"",
        ];
        let expected = run("git", &superproject, &args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_summary_clean_cases_match_upstream_git() {
    let root = unique_temp_dir("submodule-summary-clean");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        let child_branch = String::from_utf8(run_success(
            "git",
            &child,
            &["symbolic-ref", "--short", "HEAD"],
        ))
        .expect("branch is utf8")
        .trim()
        .to_string();
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "add submodule",
            ],
        );

        for args in [
            vec!["submodule", "summary"],
            vec!["submodule", "summary", "deps/child"],
            vec!["submodule", "summary", "--cached"],
            vec!["submodule", "summary", "--files"],
            vec!["submodule", "summary", "--summary-limit", "1"],
            vec!["submodule", "summary", "--summary-limit=1"],
            vec!["submodule", "summary", "missing"],
            vec!["submodule", "summary", "--no-files"],
            vec!["submodule", "summary", "--cached", "--files"],
            vec!["submodule", "summary", "--summary-limit"],
            vec!["submodule", "summary", "--summary-limit=x"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
        run_success("git", &superproject, &["add", "deps/child"]);
        for args in [
            vec!["submodule", "summary", "--cached"],
            vec!["submodule", "summary", "--files"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
        run_success(
            "git",
            &superproject,
            &["reset", "-q", "HEAD", "--", "deps/child"],
        );

        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child update",
            ],
        );
        run_success("git", &superproject.join("deps/child"), &["fetch", "-q"]);
        let remote_branch = format!("origin/{child_branch}");
        run_success(
            "git",
            &superproject.join("deps/child"),
            &["checkout", "-q", &remote_branch],
        );
        for args in [
            vec!["submodule", "summary"],
            vec!["submodule", "summary", "deps/child"],
            vec!["submodule", "summary", "--cached"],
            vec!["submodule", "summary", "--files"],
            vec!["submodule", "summary", "--summary-limit", "1"],
            vec!["submodule", "summary", "--summary-limit", "0"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            "git",
            &superproject.join("deps/child"),
            &["checkout", "-q", "HEAD~1"],
        );
        for args in [
            vec!["submodule", "summary"],
            vec!["submodule", "summary", "deps/child"],
            vec!["submodule", "summary", "--summary-limit", "1"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }

        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "second child update",
            ],
        );
        run_success("git", &superproject.join("deps/child"), &["fetch", "-q"]);
        run_success(
            "git",
            &superproject.join("deps/child"),
            &["checkout", "-q", &remote_branch],
        );
        for args in [
            vec!["submodule", "summary"],
            vec!["submodule", "summary", "--summary-limit", "1"],
            vec!["submodule", "summary", "--summary-limit=2"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_absorbgitdirs_migrates_embedded_gitdir_like_upstream_git() {
    let root = unique_temp_dir("submodule-absorbgitdirs");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );

        let noop_args = ["submodule", "absorbgitdirs"];
        let expected = run("git", &superproject, &noop_args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &noop_args);
        assert_same_output(actual, expected, &noop_args);

        embed_submodule_git_dir(&superproject, "deps/child");
        let args = ["submodule", "absorbgitdirs", "deps/child"];
        let expected = run("git", &superproject, &args);
        let expected_gitfile =
            fs::read(superproject.join("deps/child/.git")).expect("read expected gitfile");
        let expected_worktree = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".git/modules/deps/child/config",
                "core.worktree",
            ],
        );

        embed_submodule_git_dir(&superproject, "deps/child");
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_gitfile =
            fs::read(superproject.join("deps/child/.git")).expect("read actual gitfile");
        let actual_worktree = run_success(
            "git",
            &superproject,
            &[
                "config",
                "--file",
                ".git/modules/deps/child/config",
                "core.worktree",
            ],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(actual_gitfile, expected_gitfile);
        assert_eq!(actual_worktree, expected_worktree);

        for args in [
            vec!["submodule", "absorbgitdirs", "--", "deps/child"],
            vec!["submodule", "absorbgitdirs", "missing"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn submodule_deinit_unregisters_local_config_like_upstream_git() {
    let root = unique_temp_dir("submodule-deinit");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        run_success("git", &child, &["init", "-q"]);
        run_success(
            "git",
            &child,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "child",
            ],
        );
        run_success("git", &superproject, &["init", "-q"]);
        let child_url = child.to_string_lossy().into_owned();
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &child_url,
                "deps/child",
            ],
        );
        run_success(
            "git",
            &superproject,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-qam",
                "add submodule",
            ],
        );

        let args = ["submodule", "deinit", "-f", "deps/child"];
        let expected = run("git", &superproject, &args);
        let expected_config = run(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        let expected_entries = fs::read_dir(superproject.join("deps/child"))
            .expect("read expected cleared submodule")
            .count();

        run_success(
            "git",
            &superproject,
            &["submodule", "update", "--init", "-q"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
        let actual_config = run(
            "git",
            &superproject,
            &["config", "--local", "--get-regexp", "^submodule"],
        );
        let actual_entries = fs::read_dir(superproject.join("deps/child"))
            .expect("read actual cleared submodule")
            .count();
        assert_same_output(actual, expected, &args);
        assert_same_output(actual_config, expected_config, &["config", "--get-regexp"]);
        assert_eq!(actual_entries, expected_entries);

        run_success(
            "git",
            &superproject,
            &["submodule", "update", "--init", "-q"],
        );
        let quiet_args = ["submodule", "deinit", "-q", "-f", "deps/child"];
        let expected = run("git", &superproject, &quiet_args);
        run_success(
            "git",
            &superproject,
            &["submodule", "update", "--init", "-q"],
        );
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &quiet_args);
        assert_same_output(actual, expected, &quiet_args);

        run_success(
            "git",
            &superproject,
            &["submodule", "update", "--init", "-q"],
        );
        fs::write(superproject.join("deps/child/dirty"), b"dirty\n")
            .expect("write dirty submodule file");
        let dirty_args = ["submodule", "deinit", "deps/child"];
        let expected = run("git", &superproject, &dirty_args);
        let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &dirty_args);
        assert_same_output(actual, expected, &dirty_args);
        let _ = fs::remove_file(superproject.join("deps/child/dirty"));

        for args in [
            vec!["submodule", "deinit"],
            vec!["submodule", "deinit", "missing"],
        ] {
            let expected = run("git", &superproject, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &superproject, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
