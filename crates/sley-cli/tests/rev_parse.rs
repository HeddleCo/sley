use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "lock") {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            match fs::copy(&path, &target) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
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

fn sley(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::sley_bin!(), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
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
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let args = ["rev-parse", "--is-shallow-repository"];

        assert_eq!(sley(&root, &args), git(&root, &args));
        let superproject = ["rev-parse", "--show-superproject-working-tree"];
        assert_eq!(sley(&root, &superproject), git(&root, &superproject));

        fs::write(root.join(".git").join("shallow"), b"").expect("write shallow marker");
        assert_eq!(sley(&root, &args), git(&root, &args));

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
        fs::write(bare.join("shallow"), b"").expect("write bare shallow marker");
        assert_eq!(sley(&bare, &args), git(&bare, &args));
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_git_common_dir_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-git-common-dir");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        fs::create_dir_all(&nested).expect("create nested worktree dir");

        for cwd in [&root, &nested] {
            for args in [
                ["rev-parse", "--git-dir"],
                ["rev-parse", "--absolute-git-dir"],
                ["rev-parse", "--git-common-dir"],
            ] {
                assert_eq!(
                    sley(cwd, &args),
                    git(cwd, &args),
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&bare, &bare_nested] {
            for args in [
                ["rev-parse", "--git-dir"],
                ["rev-parse", "--absolute-git-dir"],
                ["rev-parse", "--git-common-dir"],
            ] {
                assert_eq!(
                    sley(cwd, &args),
                    git(cwd, &args),
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_inside_repository_flags_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-inside-flags");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                ["rev-parse", "--is-inside-work-tree"],
                ["rev-parse", "--is-inside-git-dir"],
                ["rev-parse", "--is-bare-repository"],
            ] {
                assert_eq!(
                    sley(cwd, &args),
                    git(cwd, &args),
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

// Mirrors upstream t1500-rev-parse.sh's `test_rev_parse` matrix (lines 92-108):
// explicit `core.bare = true/false/unset` config, optionally combined with a
// GIT_DIR pointing at the worktree's `.git` or a detached `repo.git` copy.
// git's rules:
//   * a bare repository is never inside a work tree (`--is-inside-work-tree`=false),
//   * with `core.bare` unset, bareness is only inferred from directory layout
//     during discovery; an explicit GIT_DIR defaults to non-bare.
#[test]
fn rev_parse_core_bare_config_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-core-bare");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "abc",
            ],
        );
        // A detached copy of `.git`, mirroring upstream's `cp -R .git repo.git`.
        let repo_git = root.join("repo.git");
        copy_dir_recursive(&root.join(".git"), &repo_git).expect("copy .git to repo.git");
        let work = root.join("work");
        fs::create_dir_all(&work).expect("create work dir");

        let flags = [
            ["rev-parse", "--is-bare-repository"],
            ["rev-parse", "--is-inside-git-dir"],
            ["rev-parse", "--is-inside-work-tree"],
        ];

        // Set `core.bare` to the requested state on the repo whose config the
        // active GIT_DIR resolves to, then compare sley to the oracle for each
        // flag under the same cwd + env.
        let check = |bare: Option<bool>,
                     config_dir: &Path,
                     cwd: &Path,
                     env_git_dir: Option<&str>| {
            match bare {
                Some(true) => {
                    git(config_dir, &["config", "core.bare", "true"]);
                }
                Some(false) => {
                    git(config_dir, &["config", "core.bare", "false"]);
                }
                None => {
                    // Tolerate an already-unset key (exit 5), like `test_unconfig`.
                    let _ = run_status(
                        sley_testkit::oracle_git(),
                        config_dir,
                        &["config", "--unset", "core.bare"],
                    );
                }
            }
            for args in &flags {
                let (expected, actual) = match env_git_dir {
                    Some(git_dir) => {
                        let envs = [("GIT_DIR", git_dir)];
                        (
                            run_status_with_env(sley_testkit::oracle_git(), cwd, args, &envs),
                            run_status_with_env(sley_testkit::sley_bin!(), cwd, args, &envs),
                        )
                    }
                    None => (
                        run_status(sley_testkit::oracle_git(), cwd, args),
                        run_status(sley_testkit::sley_bin!(), cwd, args),
                    ),
                };
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} (core.bare={bare:?}, GIT_DIR={env_git_dir:?}) in {}",
                    cwd.display()
                );
            }
        };

        // toplevel: core.bare true / unset (upstream L92, L94)
        check(Some(true), &root, &root, None);
        check(None, &root, &root, None);

        // GIT_DIR=../.git from work: core.bare false / true / unset (upstream L97-101)
        check(Some(false), &root, &work, Some("../.git"));
        check(Some(true), &root, &work, Some("../.git"));
        check(None, &root, &work, Some("../.git"));

        // GIT_DIR=../repo.git from work: core.bare false / true / unset (upstream L104-108)
        check(Some(false), &repo_git, &work, Some("../repo.git"));
        check(Some(true), &repo_git, &work, Some("../repo.git"));
        check(None, &repo_git, &work, Some("../repo.git"));
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_worktree_path_options_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-worktree-path-options");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        for cwd in [&root, &nested, &git_root, &git_nested, &bare, &bare_nested] {
            for args in [
                ["rev-parse", "--show-toplevel"],
                ["rev-parse", "--show-prefix"],
                ["rev-parse", "--show-cdup"],
            ] {
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_show_ref_format_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-show-ref-format");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        fs::create_dir_all(&nested).expect("create nested worktree dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
        let bare_nested = bare.join("objects").join("probe");
        fs::create_dir_all(&bare_nested).expect("create nested bare dir");

        let args = ["rev-parse", "--show-ref-format"];
        for cwd in [&root, &nested, &bare, &bare_nested] {
            assert_eq!(
                sley(cwd, &args),
                git(cwd, &args),
                "sley result differed for {args:?} in {}",
                cwd.display()
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_submodule_gitfile_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-submodule-gitfile");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let child = root.join("child");
        let superproject = root.join("super");
        fs::create_dir_all(&child).expect("create child repo");
        fs::create_dir_all(&superproject).expect("create superproject repo");
        git(&child, &["init", "-q", "-b", "main"]);
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
        git(&superproject, &["init", "-q", "-b", "main"]);
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
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_path_format_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-path-format");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
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
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_git_path_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-git-path");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let nested = root.join("sub").join("dir");
        let git_root = root.join(".git");
        let git_nested = git_root.join("objects").join("probe");
        fs::create_dir_all(&nested).expect("create nested worktree dir");
        fs::create_dir_all(&git_nested).expect("create nested git dir");

        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
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
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }

        let object_dir = root.join("custom-objects");
        let index_dir = root.join("custom-index-dir");
        fs::create_dir_all(&object_dir).expect("create custom object dir");
        fs::create_dir_all(&index_dir).expect("create custom index dir");
        git(&root, &["--git-dir=custom-common", "init", "-q"]);
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
            (
                vec!["rev-parse", "--git-path", "info/////grafts"],
                vec![("GIT_GRAFT_FILE", "custom-grafts")],
            ),
            (
                vec!["rev-parse", "--git-path", "logs/refs/heads/main"],
                vec![("GIT_COMMON_DIR", "custom-common")],
            ),
            (
                vec!["rev-parse", "--git-path", "common/file"],
                vec![("GIT_COMMON_DIR", "custom-common")],
            ),
            (
                vec!["rev-parse", "--git-path", "info/sparse-checkout"],
                vec![("GIT_COMMON_DIR", "custom-common")],
            ),
        ] {
            let expected = run_status_with_env(sley_testkit::oracle_git(), &root, &args, &envs);
            let actual = run_status_with_env(sley_testkit::sley_bin!(), &root, &args, &envs);
            assert_eq!(
                actual, expected,
                "sley result differed for {args:?} with env {envs:?}"
            );
        }

        let custom_hooks = root.join(".git").join("custom-hooks");
        fs::create_dir_all(&custom_hooks).expect("create custom hooks dir");
        git(&root, &["config", "core.hooksPath", ".git/custom-hooks"]);
        let args = vec!["rev-parse", "--git-path", "hooks/abc"];
        let expected = run_status(sley_testkit::oracle_git(), &root, &args);
        let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
        assert_eq!(actual, expected, "configured hooks path differed");

        for args in [
            vec!["rev-parse", "--git-path", "hooks/abc"],
            vec![
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "hooks/abc",
            ],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &nested, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &nested, &args);
            assert_eq!(actual, expected, "nested configured hooks path differed");
        }

        fs::write(
            root.join(".git").join("config.worktree"),
            b"[core]\n\thooksPath = .git/worktree-hooks\n",
        )
        .expect("write per-worktree config");
        git(&root, &["config", "extensions.worktreeConfig", "true"]);
        let expected = run_status(sley_testkit::oracle_git(), &nested, &args);
        let actual = run_status(sley_testkit::sley_bin!(), &nested, &args);
        assert_eq!(actual, expected, "enabled worktree config differed");

        git(&root, &["config", "extensions.worktreeConfig", "false"]);
        let expected = run_status(sley_testkit::oracle_git(), &nested, &args);
        let actual = run_status(sley_testkit::sley_bin!(), &nested, &args);
        assert_eq!(actual, expected, "disabled worktree config was not ignored");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_resolve_git_dir_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-resolve-git-dir");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "repo", "-b", "main"]);
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);
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
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_local_env_vars_and_path_format_errors_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-local-env-vars");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("create outside dir");
        let repo = root.join("repo");
        git(&root, &["init", "-q", "repo", "-b", "main"]);

        for cwd in [&outside, &repo] {
            for args in [
                vec!["rev-parse", "--local-env-vars"],
                vec!["rev-parse", "--path-format=relative", "--local-env-vars"],
                vec!["rev-parse", "--path-format=absolute", "--local-env-vars"],
                vec!["rev-parse", "--path-format", "--git-dir"],
                vec!["rev-parse", "--path-format=", "--git-dir"],
                vec!["rev-parse", "--path-format=bogus", "--git-dir"],
            ] {
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_sq_quote_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-sq-quote");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let outside = root.join("outside");
        fs::create_dir_all(&outside).expect("create outside dir");
        let repo = root.join("repo");
        git(&root, &["init", "-q", "repo", "-b", "main"]);

        for cwd in [&outside, &repo] {
            for args in [
                vec!["rev-parse", "--sq-quote"],
                vec!["rev-parse", "--sq-quote", "a", "b c", "d'e"],
                vec!["rev-parse", "--sq-quote", ""],
                vec!["rev-parse", "--sq-quote", "--", "--flag"],
            ] {
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_object_format_modes_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-object-format-modes");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let git_root = root.join(".git");
        let bare = root.join("bare.git");
        git(&root, &["init", "-q", "--bare", "bare.git", "-b", "main"]);

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
                let expected = run_status(sley_testkit::oracle_git(), cwd, &args);
                let actual = run_status(sley_testkit::sley_bin!(), cwd, &args);
                assert_eq!(
                    actual,
                    expected,
                    "sley result differed for {args:?} in {}",
                    cwd.display()
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_verify_quiet_missing_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-verify-missing");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
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
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_abbreviated_object_ids_match_upstream_git() {
    let root = unique_temp_dir("rev-parse-abbrev-object-id");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("payload.txt"), b"payload\n").expect("write payload");
        git(&root, &["add", "payload.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial",
                "-q",
            ],
        );
        let head = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("HEAD is utf8")
            .trim()
            .to_string();
        let loose_prefix = &head[..8];
        assert_eq!(
            sley(&root, &["rev-parse", loose_prefix]),
            git(&root, &["rev-parse", loose_prefix])
        );

        git(&root, &["gc", "--quiet"]);
        let packed_prefix = &head[..10];
        assert_eq!(
            sley(&root, &["rev-parse", packed_prefix]),
            git(&root, &["rev-parse", packed_prefix])
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_hex_refname_prefers_ref_and_warns_like_upstream_git() {
    let root = unique_temp_dir("rev-parse-hex-refname");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("first.txt"), b"first\n").expect("write first");
        git(&root, &["add", "first.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "first",
                "-q",
            ],
        );
        let first = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("first HEAD is utf8")
            .trim()
            .to_string();
        let hex_ref = &first[..4];
        fs::write(root.join("second.txt"), b"second\n").expect("write second");
        git(&root, &["add", "second.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second",
                "-q",
            ],
        );
        git(&root, &["branch", hex_ref, "HEAD"]);

        let args = ["rev-parse", hex_ref];
        let expected = run_status(sley_testkit::oracle_git(), &root, &args);
        let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
        assert_eq!(actual, expected);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn rev_parse_parent_suffixes_use_upstream_commit_graph() {
    let root = unique_temp_dir("rev-parse-commit-graph");
    fs::create_dir_all(&root).expect("create temp root");
    {
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
                sley(&root, &args),
                git(&root, &args),
                "sley result differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

fn rev_parse_terminates(cwd: &Path, args: &[&str]) -> String {
    let mut child = Command::new(sley_testkit::sley_bin!())
        .current_dir(cwd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sley");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let mut out = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
            }
            assert!(
                status.success(),
                "sley {args:?} exited with {:?}",
                status.code()
            );
            return out.trim().to_string();
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "sley {args:?} did not terminate within 10s (rev-parse infinite-loop regression)"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn setup_rev_parse_repo(dir: &Path, sha256: bool) {
    fs::create_dir_all(dir).expect("create repo dir");
    if sha256 {
        run(
            sley_testkit::oracle_git(),
            dir,
            &["init", "-q", "--object-format=sha256", "-b", "main"],
        );
    } else {
        run(
            sley_testkit::oracle_git(),
            dir,
            &["init", "-q", "-b", "main"],
        );
    }
    run(
        sley_testkit::oracle_git(),
        dir,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "c1",
            "-q",
        ],
    );
}

// Regression: `rev-parse --short=N` / `--abbrev-ref` / `--symbolic-full-name` used
// to spin forever (a `continue` in the arg loop never advanced the index).
// `--short=N` with N >= the hash hex length must print the full hash, like git.
#[test]
fn rev_parse_short_and_ref_flags_terminate_and_match_git() {
    let root = unique_temp_dir("rev-parse-short");
    let sha1 = root.join("sha1");
    setup_rev_parse_repo(&sha1, false);
    for args in [
        ["rev-parse", "--short=7", "HEAD"],
        ["rev-parse", "--short=40", "HEAD"],
        ["rev-parse", "--short=100", "HEAD"],
        ["rev-parse", "--abbrev-ref", "HEAD"],
        ["rev-parse", "--symbolic-full-name", "HEAD"],
    ] {
        let actual = rev_parse_terminates(&sha1, &args);
        let expected = String::from_utf8(git(&sha1, &args)).expect("utf8");
        assert_eq!(actual, expected.trim(), "rev-parse {args:?} mismatch");
    }

    let sha256 = root.join("sha256");
    setup_rev_parse_repo(&sha256, true);
    for args in [
        ["rev-parse", "--short=64", "HEAD"],
        ["rev-parse", "--short=100", "HEAD"],
    ] {
        let actual = rev_parse_terminates(&sha256, &args);
        let expected = String::from_utf8(git(&sha256, &args)).expect("utf8");
        assert_eq!(
            actual,
            expected.trim(),
            "rev-parse {args:?} mismatch (sha256)"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// t1504-ceiling-dirs: a leading empty `GIT_CEILING_DIRECTORIES` entry disables
/// realpath resolution so a ceiling named via a symlink (`top` → `sub`) does
/// not hide the real path when discovering from `sub/dir`.
#[test]
fn rev_parse_show_prefix_ceiling_symlink_no_resolve_matches_upstream_git() {
    let root = unique_temp_dir("rev-parse-ceiling-no-resolve");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        let sub_dir = root.join("sub").join("dir");
        fs::create_dir_all(&sub_dir).expect("create sub/dir");
        let top = root.join("top");
        #[cfg(unix)]
        std::os::unix::fs::symlink("sub", &top).expect("symlink top -> sub");
        #[cfg(not(unix))]
        {
            let _ = top;
            return;
        }

        let top_s = top.to_string_lossy().into_owned();
        let top_slash = format!("{top_s}/");
        let resolved_ceiling = root.join("sub").to_string_lossy().into_owned();

        // With realpath: ceiling at the symlink target blocks discovery.
        for (label, ceiling) in [
            ("resolved_symlink_target", resolved_ceiling.as_str()),
            ("resolved_via_symlink_name", top_s.as_str()),
            ("resolved_via_symlink_name_slash", top_slash.as_str()),
        ] {
            let envs = [("GIT_CEILING_DIRECTORIES", ceiling)];
            let expected = run_status_with_env(
                sley_testkit::oracle_git(),
                &sub_dir,
                &["rev-parse", "--show-prefix"],
                &envs,
            );
            let actual = run_status_with_env(
                sley_testkit::sley_bin!(),
                &sub_dir,
                &["rev-parse", "--show-prefix"],
                &envs,
            );
            assert_eq!(
                actual, expected,
                "sley result differed for resolved ceiling {label}={ceiling}"
            );
        }

        // Leading empty entry (`:path`) disables resolve — discovery succeeds.
        for (label, ceiling) in [
            ("no_resolve", format!(":{top_s}")),
            ("no_resolve_slash", format!(":{top_slash}")),
        ] {
            let envs = [("GIT_CEILING_DIRECTORIES", ceiling.as_str())];
            let expected = run_status_with_env(
                sley_testkit::oracle_git(),
                &sub_dir,
                &["rev-parse", "--show-prefix"],
                &envs,
            );
            let actual = run_status_with_env(
                sley_testkit::sley_bin!(),
                &sub_dir,
                &["rev-parse", "--show-prefix"],
                &envs,
            );
            assert_eq!(
                actual, expected,
                "sley result differed for no-resolve ceiling {label}={ceiling}"
            );
            assert_eq!(
                expected.0, 0,
                "oracle should succeed for no-resolve {label}"
            );
            assert_eq!(
                String::from_utf8_lossy(&expected.1),
                "sub/dir/\n",
                "oracle show-prefix for no-resolve {label}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}
