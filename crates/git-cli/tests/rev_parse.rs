use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

fn run_status(program: &str, cwd: &Path, args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    (
        output.status.code().expect("process terminated by signal"),
        output.stdout,
        output.stderr,
    )
}

fn run_status_with_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    (
        output.status.code().expect("process terminated by signal"),
        output.stdout,
        output.stderr,
    )
}

#[test]
fn rev_parse_is_shallow_repository_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-shallow");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let args = ["rev-parse", "--is-shallow-repository"];

        assert_eq!(git_rs(&root, &args), git(&root, &args));
        let superproject = ["rev-parse", "--show-superproject-working-tree"];
        assert_eq!(git_rs(&root, &superproject), git(&root, &superproject));

        fs::write(root.join(".git").join("shallow"), b"").expect("write shallow marker");
        assert_eq!(git_rs(&root, &args), git(&root, &args));

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        fs::write(bare.join("shallow"), b"").expect("write bare shallow marker");
        assert_eq!(git_rs(&bare, &args), git(&bare, &args));
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_git_common_dir_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-git-common-dir");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        fs::create_dir_all(&nested).expect("create nested worktree dir");

        for cwd in [&root, &nested] {
            for args in [
                ["rev-parse", "--git-dir"],
                ["rev-parse", "--absolute-git-dir"],
                ["rev-parse", "--git-common-dir"],
            ] {
                assert_eq!(
                    git_rs(cwd, &args),
                    git(cwd, &args),
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&bare, &bare_nested] {
            for args in [
                ["rev-parse", "--git-dir"],
                ["rev-parse", "--absolute-git-dir"],
                ["rev-parse", "--git-common-dir"],
            ] {
                assert_eq!(
                    git_rs(cwd, &args),
                    git(cwd, &args),
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_inside_repository_flags_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-inside-flags");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                ["rev-parse", "--is-inside-work-tree"],
                ["rev-parse", "--is-inside-git-dir"],
                ["rev-parse", "--is-bare-repository"],
            ] {
                assert_eq!(
                    git_rs(cwd, &args),
                    git(cwd, &args),
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_worktree_path_options_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-worktree-path-options");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                ["rev-parse", "--show-toplevel"],
                ["rev-parse", "--show-prefix"],
                ["rev-parse", "--show-cdup"],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_show_ref_format_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-show-ref-format");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        fs::create_dir_all(&nested).expect("create nested worktree dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        let args = ["rev-parse", "--show-ref-format"];
        for cwd in [&root, &nested, &bare, &bare_nested] {
            assert_eq!(
                git_rs(cwd, &args),
                git(cwd, &args),
                "git-rs result differed for {args:?} in {}",
                cwd.display()
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_submodule_gitfile_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-submodule-gitfile");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        git(&child, &["init", "-q"]);
        git(
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
        git(&superproject, &["init", "-q"]);
        git(
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

        let submodule = superproject.join("sub");
        let nested = submodule.join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested submodule dir");

        for cwd in [&submodule, &nested] {
            for args in [
                vec!["rev-parse", "--git-dir"],
                vec!["rev-parse", "--absolute-git-dir"],
                vec!["rev-parse", "--git-common-dir"],
                vec!["rev-parse", "--show-toplevel"],
                vec!["rev-parse", "--path-format=relative", "--show-toplevel"],
                vec!["rev-parse", "--show-prefix"],
                vec!["rev-parse", "--show-cdup"],
                vec!["rev-parse", "--show-superproject-working-tree"],
                vec!["rev-parse", "--is-inside-work-tree"],
                vec!["rev-parse", "--is-inside-git-dir"],
                vec!["rev-parse", "--is-bare-repository"],
                vec!["rev-parse", "--git-path", "index"],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_path_format_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-path-format");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                vec!["rev-parse", "--path-format=absolute", "--git-dir"],
                vec!["rev-parse", "--path-format=relative", "--git-dir"],
                vec!["rev-parse", "--path-format=absolute", "--git-common-dir"],
                vec!["rev-parse", "--path-format=relative", "--git-common-dir"],
                vec!["rev-parse", "--path-format=absolute", "--absolute-git-dir"],
                vec!["rev-parse", "--path-format=relative", "--absolute-git-dir"],
                vec!["rev-parse", "--path-format=absolute", "--show-toplevel"],
                vec!["rev-parse", "--path-format=relative", "--show-toplevel"],
                vec![
                    "rev-parse",
                    "--path-format=relative",
                    "--git-dir",
                    "--path-format=absolute",
                    "--git-common-dir",
                ],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_git_path_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-git-path");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                vec!["rev-parse", "--git-path", "objects/aa/bb"],
                vec!["rev-parse", "--git-path", "index"],
                vec![
                    "rev-parse",
                    "--path-format=relative",
                    "--git-path",
                    "objects",
                ],
                vec![
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects/aa/bb",
                ],
                vec!["rev-parse", "--git-path"],
                vec!["rev-parse", "--git-path", "--git-dir"],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }

        let object_dir = root.join("custom-objects");
        let index_dir = root.join("custom-index-dir");
        fs::create_dir_all(&object_dir).expect("create custom object dir");
        fs::create_dir_all(&index_dir).expect("create custom index dir");
        for (args, envs) in [
            (
                vec!["rev-parse", "--git-path", "objects/aa/bb"],
                vec![("GIT_OBJECT_DIRECTORY", "custom-objects")],
            ),
            (
                vec![
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects/aa/bb",
                ],
                vec![("GIT_OBJECT_DIRECTORY", "custom-objects")],
            ),
            (
                vec![
                    "rev-parse",
                    "--path-format=relative",
                    "--git-path",
                    "objects/aa/bb",
                ],
                vec![("GIT_OBJECT_DIRECTORY", "custom-objects")],
            ),
            (
                vec!["rev-parse", "--git-path", "objects"],
                vec![("GIT_OBJECT_DIRECTORY", "custom-objects")],
            ),
            (
                vec!["rev-parse", "--git-path", "index"],
                vec![("GIT_INDEX_FILE", "custom-index-dir/index")],
            ),
            (
                vec!["rev-parse", "--path-format=absolute", "--git-path", "index"],
                vec![("GIT_INDEX_FILE", "custom-index-dir/index")],
            ),
            (
                vec!["rev-parse", "--path-format=relative", "--git-path", "index"],
                vec![("GIT_INDEX_FILE", "custom-index-dir/index")],
            ),
        ] {
            let expected = run_status_with_env("git", &root, &args, &envs);
            let actual = run_status_with_env(env!("CARGO_BIN_EXE_git-rs"), &root, &args, &envs);
            assert_eq!(
                actual, expected,
                "git-rs result differed for {args:?} with env {envs:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_resolve_git_dir_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-resolve-git-dir");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q", "repo"]);
        git(&root, &["init", "-q", "--bare", "bare.git"]);
        let repo = root.join("repo");
        let nested = repo.join("sub").join("dir");
        let outside = root.join("outside");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&outside).expect("create outside dir");

        for cwd in [&repo, &nested, &outside, &root.join("bare.git")] {
            for args in [
                vec!["rev-parse", "--resolve-git-dir", ".git"],
                vec!["rev-parse", "--resolve-git-dir", "../repo/.git"],
                vec!["rev-parse", "--resolve-git-dir", "../bare.git"],
                vec!["rev-parse", "--resolve-git-dir", "missing"],
                vec!["rev-parse", "--resolve-git-dir"],
                vec![
                    "rev-parse",
                    "--resolve-git-dir",
                    "../repo/.git",
                    "--resolve-git-dir",
                    "../bare.git",
                ],
                vec![
                    "rev-parse",
                    "--local-env-vars",
                    "--resolve-git-dir",
                    "../repo/.git",
                ],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_local_env_vars_and_path_format_errors_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-local-env-vars");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("create outside dir");
        let repo = root.join("repo");
        git(&root, &["init", "-q", "repo"]);

        for cwd in [&outside, &repo] {
            for args in [
                vec!["rev-parse", "--local-env-vars"],
                vec!["rev-parse", "--path-format=relative", "--local-env-vars"],
                vec!["rev-parse", "--path-format=absolute", "--local-env-vars"],
                vec!["rev-parse", "--path-format", "--git-dir"],
                vec!["rev-parse", "--path-format=", "--git-dir"],
                vec!["rev-parse", "--path-format=bogus", "--git-dir"],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_sq_quote_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-sq-quote");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("create outside dir");
        let repo = root.join("repo");
        git(&root, &["init", "-q", "repo"]);

        for cwd in [&outside, &repo] {
            for args in [
                vec!["rev-parse", "--sq-quote"],
                vec!["rev-parse", "--sq-quote", "a", "b c", "d'e"],
                vec!["rev-parse", "--sq-quote", ""],
                vec!["rev-parse", "--sq-quote", "--", "--flag"],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_object_format_modes_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-object-format-modes");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        let git_root = root.join(".git");
        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git"]);

        for cwd in [&root, &git_root, &bare] {
            for args in [
                vec![
                    "rev-parse",
                    "--show-object-format=storage",
                    "--show-object-format=input",
                    "--show-object-format=output",
                ],
                vec![
                    "rev-parse",
                    "--show-object-format=storage",
                    "--show-ref-format",
                ],
                vec!["rev-parse", "--show-object-format=bogus"],
                vec!["rev-parse", "--show-object-format="],
            ] {
                let expected = run_status("git", cwd, &args);
                let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "git-rs result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_verify_quiet_missing_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-verify-missing");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["rev-parse", "--verify", "missing"],
            vec!["rev-parse", "--verify", "--quiet", "missing"],
            vec!["rev-parse", "--verify", "-q", "missing"],
            vec!["rev-parse", "--verify"],
            vec!["rev-parse", "--verify", "--quiet"],
            vec!["rev-parse", "--verify", "HEAD", "--"],
            vec!["rev-parse", "--verify", "--", "HEAD"],
            vec!["rev-parse", "--verify", "--quiet", "--", "HEAD"],
            vec!["rev-parse", "--verify", "--end-of-options", "HEAD"],
        ] {
            let expected = run_status("git", &root, &args);
            let actual = run_status(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_eq!(actual, expected, "git-rs result differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn rev_parse_parent_suffixes_use_upstream_commit_graph() {
    let root = unique_temp_dir("rev-parse-commit-graph");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("base.txt"), b"base\n").expect("write base");
        git(&root, &["add", "base.txt"]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        );
        git(&root, &["checkout", "-q", "-b", "side"]);
        fs::write(root.join("side.txt"), b"side\n").expect("write side");
        git(&root, &["add", "side.txt"]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "side"],
        );
        git(&root, &["checkout", "-q", "main"]);
        fs::write(root.join("main.txt"), b"main\n").expect("write main");
        git(&root, &["add", "main.txt"]);
        git(
            &root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "main"],
        );
        git(
            &root,
            &[
                "-c",
                "commit.gpgsign=false",
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "merge side",
                "side",
            ],
        );
        git(&root, &["commit-graph", "write", "--reachable"]);
        assert!(
            root.join(".git")
                .join("objects")
                .join("info")
                .join("commit-graph")
                .exists(),
            "upstream did not write commit-graph"
        );

        for args in [
            vec!["rev-parse", "HEAD^", "HEAD^1", "HEAD^2"],
            vec!["rev-parse", "HEAD~", "HEAD~1", "HEAD~2", "HEAD^^"],
            vec!["rev-parse", "HEAD^2~1"],
        ] {
            assert_eq!(
                git_rs(&root, &args),
                git(&root, &args),
                "git-rs result differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
