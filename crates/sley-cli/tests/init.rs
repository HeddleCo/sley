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

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
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
        let rust_stdout = git_rs(&root, &rust_args);
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
        let expected = run_status(sley_testkit::oracle_git(), &root, &["init", "-b", "topic", repo_arg]);
        assert_eq!(expected.0, 0, "upstream init failed");
        fs::remove_dir_all(&repo).expect("remove upstream repo");
        let actual = run_status(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["init", "-b", "topic", repo_arg],
        );
        assert_eq!(actual, expected, "fresh non-bare init differed");
        assert_eq!(git(&repo, &["symbolic-ref", "HEAD"]), b"refs/heads/topic\n");

        let bare = root.join("bare.git");
        let bare_arg = bare.to_str().expect("utf8 temp path");
        let expected = run_status(sley_testkit::oracle_git(), &root, &["init", "--bare", "-b", "topic", bare_arg]);
        assert_eq!(expected.0, 0, "upstream bare init failed");
        fs::remove_dir_all(&bare).expect("remove upstream bare repo");
        let actual = run_status(
            env!("CARGO_BIN_EXE_sley"),
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
        let expected = run_status(sley_testkit::oracle_git(), &root, &["init", "-b", "other", reinit_arg]);
        let expected_head = git(&reinit, &["symbolic-ref", "HEAD"]);
        fs::remove_dir_all(&reinit).expect("remove upstream reinit repo");
        git_rs(&root, &["init", "-q", "-b", "topic", reinit_arg]);
        let actual = run_status(
            env!("CARGO_BIN_EXE_sley"),
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
        let expected = run_status(sley_testkit::oracle_git(), &root, &["init", "--bare", bare_reinit_arg]);
        let expected_head = git(
            &root,
            &["--git-dir", bare_reinit_arg, "symbolic-ref", "HEAD"],
        );
        fs::remove_dir_all(&bare_reinit).expect("remove upstream bare reinit repo");
        git_rs(
            &root,
            &["init", "-q", "--bare", "-b", "topic", bare_reinit_arg],
        );
        let actual = run_status(
            env!("CARGO_BIN_EXE_sley"),
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
        let mut upstream_args = vec!["init", "-q"];
        upstream_args.extend_from_slice(&extra_args);
        upstream_args.push(upstream_path);
        git(&root, &upstream_args);
        let mut rust_args = vec!["init", "-q"];
        rust_args.extend_from_slice(&extra_args);
        rust_args.push(rust_path);
        git_rs(&root, &rust_args);

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
    let templatedir = format!("~/templates");
    let config_arg = format!("init.templatedir={templatedir}");
    let upstream_path = upstream.to_str().expect("utf8 temp path");
    let rust_path = rust.to_str().expect("utf8 temp path");

    for (program, path) in [
        (sley_testkit::oracle_git(), upstream_path),
        (env!("CARGO_BIN_EXE_sley"), rust_path),
    ] {
        let output = Command::new(program)
            .current_dir(&root)
            .env("HOME", &fake_home)
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
    git_rs(&root, &args);

    let expected_gitfile = fs::read_to_string(worktree.join(".git")).expect("upstream gitfile");
    let actual_gitfile = fs::read_to_string(worktree.join(".git")).expect("rust gitfile");
    assert_eq!(actual_gitfile, expected_gitfile, "gitfile differed");
    assert_eq!(
        git(&worktree, &["symbolic-ref", "HEAD"]),
        git_rs(&worktree, &["symbolic-ref", "HEAD"]),
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
        ("shared", vec!["init", "-q", "--shared=group"]),
        ("sha256-env", vec!["init", "-q"]),
    ] {
        let upstream = root.join(format!("git-{name}"));
        let rust = root.join(format!("rust-{name}"));
        let mut upstream_args = args.clone();
        upstream_args.push(upstream.to_str().expect("utf8 temp path"));
        let mut rust_args = args;
        rust_args.push(rust.to_str().expect("utf8 temp path"));
        let mut upstream_cmd = Command::new(sley_testkit::oracle_git());
        upstream_cmd.current_dir(&root).args(&upstream_args);
        let mut rust_cmd = Command::new(env!("CARGO_BIN_EXE_sley"));
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
                git_rs(&rust, &cmd),
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
    git_rs(&root, &["init", "-q", "--ref-format=reftable", rust_arg]);

    for cmd in [
        vec!["rev-parse", "--show-ref-format"],
        vec!["config", "--get", "extensions.refStorage"],
        vec!["symbolic-ref", "HEAD"],
    ] {
        assert_eq!(
            git_rs(&rust, &cmd),
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
    let expected = run_status(sley_testkit::oracle_git(), &root, &["init", "--ref-format=garbage", repo_arg]);
    let actual = run_status(
        env!("CARGO_BIN_EXE_sley"),
        &root,
        &["init", "--ref-format=garbage", repo_arg],
    );
    assert_eq!(actual, expected, "unknown ref format init differed");
    let _ = fs::remove_dir_all(&root);
}
