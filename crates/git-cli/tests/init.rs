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
    run(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

#[test]
fn init_initial_branch_matches_upstream_git_head() {
    let root = unique_temp_dir("init-initial-branch");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
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
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn init_bare_stdout_and_reinit_match_upstream_git() {
    let root = unique_temp_dir("init-bare-reinit");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        let repo = root.join("repo");
        let repo_arg = repo.to_str().expect("utf8 temp path");
        let expected = run_status("git", &root, &["init", "-b", "topic", repo_arg]);
        assert_eq!(expected.0, 0, "upstream init failed");
        fs::remove_dir_all(&repo).expect("remove upstream repo");
        let actual = run_status(
            env!("CARGO_BIN_EXE_git-rs"),
            &root,
            &["init", "-b", "topic", repo_arg],
        );
        assert_eq!(actual, expected, "fresh non-bare init differed");
        assert_eq!(git(&repo, &["symbolic-ref", "HEAD"]), b"refs/heads/topic\n");

        let bare = root.join("bare.git");
        let bare_arg = bare.to_str().expect("utf8 temp path");
        let expected = run_status("git", &root, &["init", "--bare", "-b", "topic", bare_arg]);
        assert_eq!(expected.0, 0, "upstream bare init failed");
        fs::remove_dir_all(&bare).expect("remove upstream bare repo");
        let actual = run_status(
            env!("CARGO_BIN_EXE_git-rs"),
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
        let expected = run_status("git", &root, &["init", "-b", "other", reinit_arg]);
        let expected_head = git(&reinit, &["symbolic-ref", "HEAD"]);
        fs::remove_dir_all(&reinit).expect("remove upstream reinit repo");
        git_rs(&root, &["init", "-q", "-b", "topic", reinit_arg]);
        let actual = run_status(
            env!("CARGO_BIN_EXE_git-rs"),
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
        let expected = run_status("git", &root, &["init", "--bare", bare_reinit_arg]);
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
            env!("CARGO_BIN_EXE_git-rs"),
            &root,
            &["init", "--bare", bare_reinit_arg],
        );
        let actual_head = git(
            &root,
            &["--git-dir", bare_reinit_arg, "symbolic-ref", "HEAD"],
        );
        assert_eq!(actual, expected, "bare reinit differed");
        assert_eq!(actual_head, expected_head, "bare reinit HEAD differed");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
