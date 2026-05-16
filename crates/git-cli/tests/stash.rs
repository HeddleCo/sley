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

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
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
    run_success("git", cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_success(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn prepare_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write first stash fixture");
    git(root, &["stash", "push", "-q", "-m", "one"]);
    fs::write(root.join("a.txt"), b"two\n").expect("write second stash fixture");
    git(root, &["stash", "push", "-q", "-m", "two"]);
}

#[test]
fn stash_list_matches_upstream_git() {
    let root = unique_temp_dir("stash-list");
    let result = (|| {
        prepare_stash_repo(&root);
        for args in [
            vec!["stash", "list"],
            vec!["stash", "list", "--oneline"],
            vec!["stash", "list", "--format=%gd"],
            vec!["stash", "list", "--format=%gD"],
            vec!["stash", "list", "--format=%gs"],
            vec!["stash", "list", "--pretty=format:%gs"],
            vec!["stash", "list", "--max-count=1"],
            vec!["stash", "list", "-1"],
            vec!["stash", "list", "-n", "1"],
            vec!["stash", "list", "-n1"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs stash output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_list_empty_matches_upstream_git() {
    let root = unique_temp_dir("stash-list-empty");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        for args in [
            vec!["stash", "list"],
            vec!["stash", "list", "--oneline"],
            vec!["stash", "list", "--format=%gd"],
            vec!["stash", "list", "--format=%gs"],
            vec!["stash", "list", "-1"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs empty stash output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
