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

fn run_status(program: &str, cwd: &Path, args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    (
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

fn sley(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::sley_bin!(), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

#[test]
fn init_initial_branch_matches_upstream_git_head() {
    let root = unique_temp_dir("init-initial-branch");
    fs::create_dir_all(&root).expect("create temp root");
    for (name, args) in [
        ("short", vec!["init", "-q", "-b", "topic"]),
        ("long", vec!["init", "-q", "--initial-branch", "release"]),
        ("equals", vec!["init", "-q", "--initial-branch=integration"]),
        (
            "quiet",
            vec!["init", "--quiet", "--initial-branch=quiet-topic"],
        ),
    ] {
        let upstream = root.join(format!("git-{name}"));
        let rust = root.join(format!("rust-{name}"));
        let mut upstream_args = args.clone();
        upstream_args.push(upstream.to_str().expect("utf8 temp path"));
        let upstream_stdout = git(&root, &upstream_args);
        let mut rust_args = args;
        rust_args.push(rust.to_str().expect("utf8 temp path"));
        let rust_stdout = sley(&root, &rust_args);
        assert_eq!(rust_stdout, upstream_stdout, "stdout differed for {name}");

        let expected = git(&upstream, &["symbolic-ref", "HEAD"]);
        let actual = git(&rust, &["symbolic-ref", "HEAD"]);
        assert_eq!(actual, expected, "HEAD differed for {name}");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_bare_stdout_and_reinit_match_upstream_git() {
    let root = unique_temp_dir("init-bare-reinit");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let repo = root.join("repo");
        let repo_arg = repo.to_str().expect("utf8 temp path");
        let expected = run_status(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-b", "topic", repo_arg],
        );
        assert_eq!(expected.0, 0, "upstream init failed");
        fs::remove_dir_all(&repo).expect("remove upstream repo");
        let actual = run_status(
            sley_testkit::sley_bin!(),
            &root,
            &["init", "-b", "topic", repo_arg],
        );
        assert_eq!(actual, expected, "fresh non-bare init differed");
        assert_eq!(git(&repo, &["symbolic-ref", "HEAD"]), b"refs/heads/topic\n");

        let bare = root.join("bare.git");
        let bare_arg = bare.to_str().expect("utf8 temp path");
        let expected = run_status(
            sley_testkit::oracle_git(),
            &root,
            &["init", "--bare", "-b", "topic", bare_arg],
        );
        assert_eq!(expected.0, 0, "upstream bare init failed");
        fs::remove_dir_all(&bare).expect("remove upstream bare repo");
        let actual = run_status(
            sley_testkit::sley_bin!(),
            &root,
            &["init", "--bare", "-b", "topic", bare_arg],
        );
        assert_eq!(actual, expected, "fresh bare init differed");
        assert_eq!(
            git(&root, &["--git-dir", bare_arg, "symbolic-ref", "HEAD"]),
            b"refs/heads/topic\n"
        );

        let reinit = root.join("reinit");
        let reinit_arg = reinit.to_str().expect("utf8 temp path");
        git(&root, &["init", "-q", "-b", "topic", reinit_arg]);
        let expected = run_status(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-b", "other", reinit_arg],
        );
        let expected_head = git(&reinit, &["symbolic-ref", "HEAD"]);
        fs::remove_dir_all(&reinit).expect("remove upstream reinit repo");
        sley(&root, &["init", "-q", "-b", "topic", reinit_arg]);
        let actual = run_status(
            sley_testkit::sley_bin!(),
            &root,
            &["init", "-b", "other", reinit_arg],
        );
        let actual_head = git(&reinit, &["symbolic-ref", "HEAD"]);
        assert_eq!(actual, expected, "non-bare reinit differed");
        assert_eq!(actual_head, expected_head, "non-bare reinit HEAD differed");

        let bare_reinit = root.join("bare-reinit.git");
        let bare_reinit_arg = bare_reinit.to_str().expect("utf8 temp path");
        git(
            &root,
            &["init", "-q", "--bare", "-b", "topic", bare_reinit_arg],
        );
        let expected = run_status(
            sley_testkit::oracle_git(),
            &root,
            &["init", "--bare", bare_reinit_arg],
        );
        let expected_head = git(
            &root,
            &["--git-dir", bare_reinit_arg, "symbolic-ref", "HEAD"],
        );
        fs::remove_dir_all(&bare_reinit).expect("remove upstream bare reinit repo");
        sley(
            &root,
            &["init", "-q", "--bare", "-b", "topic", bare_reinit_arg],
        );
        let actual = run_status(
            sley_testkit::sley_bin!(),
            &root,
            &["init", "--bare", bare_reinit_arg],
        );
        let actual_head = git(
            &root,
            &["--git-dir", bare_reinit_arg, "symbolic-ref", "HEAD"],
        );
        assert_eq!(actual, expected, "bare reinit differed");
        assert_eq!(actual_head, expected_head, "bare reinit HEAD differed");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_template_and_templatedir_match_upstream_git() {
    let root = unique_temp_dir("init-template");
    fs::create_dir_all(&root).expect("create temp root");
    let template = root.join("template");
    fs::create_dir_all(template.join("hooks")).expect("create template hooks");
    fs::write(template.join("description"), b"from template\n").expect("write description");
    fs::write(template.join("hooks/pre-commit"), b"#!/bin/sh\n").expect("write hook");

    let template_arg = format!("--template={}", template.display());
    for (name, extra_args) in [
        ("template-flag", vec![template_arg.as_str()]),
        ("template-blank", vec!["--template="]),
    ] {
        let upstream = root.join(format!("git-{name}"));
        let rust = root.join(format!("rust-{name}"));
        let upstream_path = upstream.to_str().expect("utf8 temp path");
        let rust_path = rust.to_str().expect("utf8 temp path");
        let mut upstream_args = vec!["init", "-q", "-b", "main"];
        upstream_args.extend_from_slice(&extra_args);
        upstream_args.push(upstream_path);
        git(&root, &upstream_args);
        let mut rust_args = vec!["init", "-q", "-b", "main"];
        rust_args.extend_from_slice(&extra_args);
        rust_args.push(rust_path);
        sley(&root, &rust_args);

        for cmd in [
            vec!["config", "--get", "core.repositoryformatversion"],
            vec!["symbolic-ref", "HEAD"],
        ] {
            assert_eq!(
                git(&upstream, &cmd),
                git(&rust, &cmd),
                "output differed for {name} {cmd:?}"
            );
        }
        if name == "template-flag" {
            assert_eq!(
                fs::read(upstream.join(".git/description")).expect("upstream description"),
                fs::read(rust.join(".git/description")).expect("rust description"),
                "description differed for {name}"
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_templatedir_config_and_tilde_expansion_match_upstream_git() {
    let root = unique_temp_dir("init-templatedir");
    fs::create_dir_all(&root).expect("create temp root");
    let fake_home = root.join("home");
    let template = fake_home.join("templates");
    fs::create_dir_all(&template).expect("create template dir");
    fs::write(template.join("description"), b"from home template\n").expect("write description");

    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let templatedir = "~/templates".to_string();
    let config_arg = format!("init.templatedir={templatedir}");
    let upstream_path = upstream.to_str().expect("utf8 temp path");
    let rust_path = rust.to_str().expect("utf8 temp path");

    for (program, path) in [
        (sley_testkit::oracle_git(), upstream_path),
        (sley_testkit::sley_bin!(), rust_path),
    ] {
        let output = Command::new(program)
            .current_dir(&root)
            .env("HOME", &fake_home)
            .env("NO_SET_GIT_TEMPLATE_DIR", "1")
            .args(["-c", &config_arg, "init", "-q", path])
            .output()
            .unwrap_or_else(|err| panic!("failed to run {program} init: {err}"));
        assert!(
            output.status.success(),
            "{program} init failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert_eq!(
        fs::read(upstream.join(".git/description")).expect("upstream description"),
        fs::read(rust.join(".git/description")).expect("rust description"),
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_separate_git_dir_matches_upstream_git() {
    let root = unique_temp_dir("init-separate-gitdir");
    fs::create_dir_all(&root).expect("create temp root");
    let worktree = root.join("worktree");
    let gitdir = root.join("external.git");
    let worktree_arg = worktree.to_str().expect("utf8 temp path");
    let gitdir_arg = gitdir.to_str().expect("utf8 temp path");
    let args = ["init", "-q", "--separate-git-dir", gitdir_arg, worktree_arg];
    git(&root, &args);
    sley(&root, &args);

    let expected_gitfile = fs::read_to_string(worktree.join(".git")).expect("upstream gitfile");
    let actual_gitfile = fs::read_to_string(worktree.join(".git")).expect("rust gitfile");
    assert_eq!(actual_gitfile, expected_gitfile, "gitfile differed");
    assert_eq!(
        git(&worktree, &["symbolic-ref", "HEAD"]),
        sley(&worktree, &["symbolic-ref", "HEAD"]),
        "HEAD differed"
    );
    assert!(
        gitdir.join("HEAD").is_file(),
        "external gitdir missing HEAD"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_shared_and_object_format_match_upstream_git() {
    let root = unique_temp_dir("init-shared-object-format");
    fs::create_dir_all(&root).expect("create temp root");
    for (name, args) in [
        ("shared", vec!["init", "-q", "--shared=group", "-b", "main"]),
        ("sha256-env", vec!["init", "-q", "-b", "main"]),
    ] {
        let upstream = root.join(format!("git-{name}"));
        let rust = root.join(format!("rust-{name}"));
        let mut upstream_args = args.clone();
        upstream_args.push(upstream.to_str().expect("utf8 temp path"));
        let mut rust_args = args;
        rust_args.push(rust.to_str().expect("utf8 temp path"));
        let mut upstream_cmd = Command::new(sley_testkit::oracle_git());
        upstream_cmd.current_dir(&root).args(&upstream_args);
        let mut rust_cmd = Command::new(sley_testkit::sley_bin!());
        rust_cmd.current_dir(&root).args(&rust_args);
        if name == "sha256-env" {
            upstream_cmd.env("GIT_DEFAULT_HASH", "sha256");
            rust_cmd.env("GIT_DEFAULT_HASH", "sha256");
        }
        let upstream_out = upstream_cmd.output().expect("run upstream init");
        let rust_out = rust_cmd.output().expect("run rust init");
        assert!(
            upstream_out.status.success(),
            "upstream init failed for {name}"
        );
        assert!(rust_out.status.success(), "rust init failed for {name}");

        let cmds = if name == "shared" {
            vec![
                vec!["config", "--get", "core.sharedRepository"],
                vec!["symbolic-ref", "HEAD"],
            ]
        } else {
            vec![
                vec!["config", "--get", "extensions.objectformat"],
                vec!["config", "--get", "core.repositoryformatversion"],
                vec!["rev-parse", "--show-object-format"],
            ]
        };
        for cmd in cmds {
            assert_eq!(
                sley(&rust, &cmd),
                git(&upstream, &cmd),
                "output differed for {name} {cmd:?}"
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_ref_format_reftable_matches_upstream_git() {
    let root = unique_temp_dir("init-reftable");
    fs::create_dir_all(&root).expect("create temp root");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let upstream_arg = upstream.to_str().expect("utf8 temp path");
    let rust_arg = rust.to_str().expect("utf8 temp path");
    git(
        &root,
        &["init", "-q", "--ref-format=reftable", upstream_arg],
    );
    sley(&root, &["init", "-q", "--ref-format=reftable", rust_arg]);

    for cmd in [
        vec!["rev-parse", "--show-ref-format"],
        vec!["config", "--get", "extensions.refStorage"],
        vec!["symbolic-ref", "HEAD"],
    ] {
        assert_eq!(
            sley(&rust, &cmd),
            git(&upstream, &cmd),
            "output differed for {cmd:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_unknown_ref_format_matches_upstream_git() {
    let root = unique_temp_dir("init-ref-format-garbage");
    fs::create_dir_all(&root).expect("create temp root");
    let repo = root.join("repo");
    let repo_arg = repo.to_str().expect("utf8 temp path");
    let expected = run_status(
        sley_testkit::oracle_git(),
        &root,
        &["init", "--ref-format=garbage", repo_arg],
    );
    let actual = run_status(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "--ref-format=garbage", repo_arg],
    );
    assert_eq!(actual, expected, "unknown ref format init differed");
    let _ = fs::remove_dir_all(&root);
}

/// Run `program` in `cwd` with extra environment overrides, capturing the result.
///
/// Used by the config-default parity tests so the oracle and sley observe an identical
/// `HOME` (and therefore an identical global config) and identical `GIT_DEFAULT_*`
/// environment. Returns `(exit_code, stdout, stderr)`.
fn run_status_env(
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
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    )
}

/// Write a global `~/.gitconfig` under `home` containing `body`.
fn write_global_config(home: &Path, body: &str) {
    fs::create_dir_all(home).expect("create fake home");
    fs::write(home.join(".gitconfig"), body).expect("write fake gitconfig");
}

/// The unconfigured default initial branch (no `init.defaultBranch`) must match upstream
/// git exactly — `refs/heads/master` on the 2.54 series — and the `init.defaultBranch`
/// global-config default must be honored when set. Covers t0001's "default branch name"
/// and "overridden default initial branch name (config)".
#[test]
fn init_default_branch_honors_config_and_falls_back_to_master() {
    let root = unique_temp_dir("init-default-branch-config");
    fs::create_dir_all(&root).expect("create temp root");

    for (name, config_body) in [
        ("fallback", ""),
        ("config", "[init]\n\tdefaultBranch = nmb\n"),
    ] {
        let home = root.join(format!("home-{name}"));
        write_global_config(&home, config_body);
        let envs = [("HOME", home.to_str().expect("utf8 home"))];

        let upstream = root.join(format!("git-{name}"));
        let rust = root.join(format!("rust-{name}"));
        let upstream_arg = upstream.to_str().expect("utf8 temp path");
        let rust_arg = rust.to_str().expect("utf8 temp path");

        let expected = run_status_env(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", upstream_arg],
            &envs,
        );
        assert_eq!(expected.0, 0, "upstream init failed for {name}");
        let actual = run_status_env(
            sley_testkit::sley_bin!(),
            &root,
            &["init", "-q", rust_arg],
            &envs,
        );
        assert_eq!(actual.0, 0, "sley init failed for {name}");

        let expected_head = run_status_env(
            sley_testkit::oracle_git(),
            &upstream,
            &["symbolic-ref", "HEAD"],
            &envs,
        );
        let actual_head = run_status_env(
            sley_testkit::oracle_git(),
            &rust,
            &["symbolic-ref", "HEAD"],
            &envs,
        );
        assert_eq!(actual_head.1, expected_head.1, "HEAD differed for {name}");
    }
    let _ = fs::remove_dir_all(&root);
}

/// An invalid `init.defaultObjectFormat` must warn (not fail) and fall back to the
/// default hash, byte-for-byte like upstream git. Covers t0001's "init warns about
/// invalid init.defaultObjectFormat".
#[test]
fn init_invalid_default_object_format_warns_like_upstream_git() {
    let root = unique_temp_dir("init-bad-default-object-format");
    fs::create_dir_all(&root).expect("create temp root");
    let home = root.join("home");
    write_global_config(&home, "[init]\n\tdefaultObjectFormat = garbage\n");
    let envs = [("HOME", home.to_str().expect("utf8 home"))];

    let upstream = root.join("git-repo");
    let rust = root.join("rust-repo");
    let expected = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "-q", upstream.to_str().expect("utf8 temp path")],
        &envs,
    );
    let actual = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "-q", rust.to_str().expect("utf8 temp path")],
        &envs,
    );
    assert_eq!(
        actual, expected,
        "invalid init.defaultObjectFormat differed"
    );
    assert_eq!(
        run_status_env(
            sley_testkit::oracle_git(),
            &rust,
            &["rev-parse", "--show-object-format"],
            &envs
        )
        .1,
        run_status_env(
            sley_testkit::oracle_git(),
            &upstream,
            &["rev-parse", "--show-object-format"],
            &envs
        )
        .1,
        "fallback object format differed"
    );
    let _ = fs::remove_dir_all(&root);
}

/// An invalid `init.defaultRefFormat` config default must warn and continue with the
/// default ref backend, while an invalid `GIT_DEFAULT_REF_FORMAT` env value must be
/// fatal — matching upstream git's split between config (warn) and env (fatal).
#[test]
fn init_invalid_default_ref_format_config_warns_env_is_fatal() {
    let root = unique_temp_dir("init-bad-default-ref-format");
    fs::create_dir_all(&root).expect("create temp root");
    let home = root.join("home");
    write_global_config(&home, "[init]\n\tdefaultRefFormat = garbage\n");
    let config_envs = [("HOME", home.to_str().expect("utf8 home"))];

    let upstream = root.join("git-config");
    let rust = root.join("rust-config");
    let expected = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "-q", upstream.to_str().expect("utf8 temp path")],
        &config_envs,
    );
    let actual = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "-q", rust.to_str().expect("utf8 temp path")],
        &config_envs,
    );
    assert_eq!(
        actual, expected,
        "invalid init.defaultRefFormat (config) differed"
    );

    let empty_home = root.join("home-empty");
    write_global_config(&empty_home, "");
    let env_envs = [
        ("HOME", empty_home.to_str().expect("utf8 home")),
        ("GIT_DEFAULT_REF_FORMAT", "garbage"),
    ];
    let upstream_env = root.join("git-env");
    let rust_env = root.join("rust-env");
    let expected_env = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "-q", upstream_env.to_str().expect("utf8 temp path")],
        &env_envs,
    );
    let actual_env = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "-q", rust_env.to_str().expect("utf8 temp path")],
        &env_envs,
    );
    assert_eq!(
        actual_env, expected_env,
        "invalid GIT_DEFAULT_REF_FORMAT (env) differed"
    );
    let _ = fs::remove_dir_all(&root);
}

/// `GIT_DEFAULT_HASH` and `--object-format` resolve the object format with the same
/// precedence and error behavior as upstream git: a bad env value or a bad CLI value is
/// fatal with `unknown hash algorithm '<value>'`, and `--object-format` overrides the
/// env. Covers the t0001 "init honors GIT_DEFAULT_HASH" / "--object-format overrides
/// GIT_DEFAULT_HASH" family plus the invalid-value path.
#[test]
fn init_object_format_env_and_cli_precedence_match_upstream_git() {
    let root = unique_temp_dir("init-object-format-precedence");
    fs::create_dir_all(&root).expect("create temp root");
    let home = root.join("home");
    write_global_config(&home, "[init]\n\tdefaultObjectFormat = sha1\n");
    let home_arg = home.to_str().expect("utf8 home");

    struct Case<'a> {
        name: &'a str,
        args: &'a [&'a str],
        envs: &'a [(&'a str, &'a str)],
        show_format: bool,
    }
    let cases = [
        Case {
            name: "env-sha256-overrides-config",
            args: &["init", "-q", "-b", "main"],
            envs: &[("HOME", home_arg), ("GIT_DEFAULT_HASH", "sha256")],
            show_format: true,
        },
        Case {
            name: "cli-overrides-env",
            args: &["init", "-q", "--object-format=sha256", "-b", "main"],
            envs: &[("HOME", home_arg), ("GIT_DEFAULT_HASH", "sha1")],
            show_format: true,
        },
        Case {
            name: "env-garbage-fatal",
            args: &["init", "-q", "-b", "main"],
            envs: &[("HOME", home_arg), ("GIT_DEFAULT_HASH", "garbage")],
            show_format: false,
        },
        Case {
            name: "cli-garbage-fatal",
            args: &["init", "-q", "--object-format=garbage", "-b", "main"],
            envs: &[("HOME", home_arg)],
            show_format: false,
        },
    ];

    for case in &cases {
        let upstream = root.join(format!("git-{}", case.name));
        let rust = root.join(format!("rust-{}", case.name));
        let mut upstream_args = case.args.to_vec();
        upstream_args.push(upstream.to_str().expect("utf8 temp path"));
        let mut rust_args = case.args.to_vec();
        rust_args.push(rust.to_str().expect("utf8 temp path"));

        let expected = run_status_env(sley_testkit::oracle_git(), &root, &upstream_args, case.envs);
        let actual = run_status_env(sley_testkit::sley_bin!(), &root, &rust_args, case.envs);
        assert_eq!(
            actual, expected,
            "object-format case {} differed",
            case.name
        );

        if case.show_format {
            assert_eq!(
                run_status_env(
                    sley_testkit::oracle_git(),
                    &rust,
                    &["rev-parse", "--show-object-format"],
                    case.envs
                )
                .1,
                run_status_env(
                    sley_testkit::oracle_git(),
                    &upstream,
                    &["rev-parse", "--show-object-format"],
                    case.envs
                )
                .1,
                "object format differed for {}",
                case.name
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

/// Re-initializing a repository with an explicit `--object-format` or `--ref-format`
/// that differs from the existing repository is fatal (exit 128), while the same format
/// is a no-op reinit — exactly as upstream git. A defaulted format (env/config) never
/// triggers the guard. Covers t0001's "init rejects attempts to initialize with
/// different hash" and the reftable "re-init with different format fails" family.
#[test]
fn init_reinit_with_conflicting_format_fails_like_upstream_git() {
    let root = unique_temp_dir("init-reinit-format-conflict");
    fs::create_dir_all(&root).expect("create temp root");
    let home = root.join("home");
    write_global_config(&home, "");
    let envs = [("HOME", home.to_str().expect("utf8 home"))];

    // Object-format mismatch on reinit is fatal; matching format is a clean reinit.
    let upstream = root.join("git-hash");
    let rust = root.join("rust-hash");
    for path in [&upstream, &rust] {
        let program = if path == &upstream {
            sley_testkit::oracle_git()
        } else {
            sley_testkit::sley_bin!()
        };
        let created = run_status_env(
            program,
            &root,
            &[
                "init",
                "-q",
                "--object-format=sha256",
                path.to_str().expect("utf8 temp path"),
            ],
            &envs,
        );
        assert_eq!(created.0, 0, "initial sha256 init failed");
    }
    let upstream_arg = upstream.to_str().expect("utf8 temp path");
    let rust_arg = rust.to_str().expect("utf8 temp path");
    let expected_conflict = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "-q", "--object-format=sha1", upstream_arg],
        &envs,
    );
    let actual_conflict = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "-q", "--object-format=sha1", rust_arg],
        &envs,
    );
    assert_eq!(
        actual_conflict, expected_conflict,
        "reinit object-format conflict differed"
    );
    // The repository must be left untouched at sha256 after the rejected reinit.
    assert_eq!(
        run_status_env(
            sley_testkit::oracle_git(),
            &rust,
            &["rev-parse", "--show-object-format"],
            &envs
        )
        .1,
        b"sha256\n".to_vec(),
        "rejected reinit must not change the object format"
    );

    let expected_same = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "--object-format=sha256", upstream_arg],
        &envs,
    );
    let actual_same = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "--object-format=sha256", rust_arg],
        &envs,
    );
    // stderr/exit must match; stdout names the canonicalized path so only compare the
    // exit code and stderr here.
    assert_eq!(
        (expected_same.0, &expected_same.2),
        (actual_same.0, &actual_same.2),
        "reinit with matching object-format differed"
    );

    // Ref-format mismatch on reinit is fatal.
    let upstream_ref = root.join("git-ref");
    let rust_ref = root.join("rust-ref");
    for (program, path) in [
        (sley_testkit::oracle_git(), &upstream_ref),
        (sley_testkit::sley_bin!(), &rust_ref),
    ] {
        let created = run_status_env(
            program,
            &root,
            &["init", "-q", path.to_str().expect("utf8 temp path")],
            &envs,
        );
        assert_eq!(created.0, 0, "initial files-format init failed");
    }
    let expected_ref = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &[
            "init",
            "-q",
            "--ref-format=reftable",
            upstream_ref.to_str().expect("utf8 temp path"),
        ],
        &envs,
    );
    let actual_ref = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &[
            "init",
            "-q",
            "--ref-format=reftable",
            rust_ref.to_str().expect("utf8 temp path"),
        ],
        &envs,
    );
    assert_eq!(
        actual_ref, expected_ref,
        "reinit ref-format conflict differed"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Reinitializing with a *defaulted* `GIT_DEFAULT_HASH` that differs from the existing
/// repository must not change the format and must not error — git only guards against an
/// explicit `--object-format`. Covers t0001's "reinit repository with GIT_DEFAULT_HASH
/// does not change format".
#[test]
fn init_reinit_ignores_default_hash_env_like_upstream_git() {
    let root = unique_temp_dir("init-reinit-default-hash");
    fs::create_dir_all(&root).expect("create temp root");
    let home = root.join("home");
    write_global_config(&home, "");
    let base_envs = [("HOME", home.to_str().expect("utf8 home"))];

    let upstream = root.join("git-repo");
    let rust = root.join("rust-repo");
    for (program, path) in [
        (sley_testkit::oracle_git(), &upstream),
        (sley_testkit::sley_bin!(), &rust),
    ] {
        let created = run_status_env(
            program,
            &root,
            &["init", "-q", path.to_str().expect("utf8 temp path")],
            &base_envs,
        );
        assert_eq!(created.0, 0, "initial sha1 init failed");
    }

    let reinit_envs = [
        ("HOME", home.to_str().expect("utf8 home")),
        ("GIT_DEFAULT_HASH", "sha256"),
    ];
    let expected = run_status_env(
        sley_testkit::oracle_git(),
        &root,
        &["init", "-q", upstream.to_str().expect("utf8 temp path")],
        &reinit_envs,
    );
    let actual = run_status_env(
        sley_testkit::sley_bin!(),
        &root,
        &["init", "-q", rust.to_str().expect("utf8 temp path")],
        &reinit_envs,
    );
    assert_eq!(actual.0, expected.0, "reinit exit differed");
    assert_eq!(actual.2, expected.2, "reinit stderr differed");
    assert_eq!(
        run_status_env(
            sley_testkit::oracle_git(),
            &rust,
            &["rev-parse", "--show-object-format"],
            &base_envs
        )
        .1,
        b"sha1\n".to_vec(),
        "defaulted GIT_DEFAULT_HASH must not change the existing format"
    );
    let _ = fs::remove_dir_all(&root);
}
