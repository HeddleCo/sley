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

fn prepare_single_stash_repo(root: &Path) {
    fs::create_dir_all(root).expect("create temp repo");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
    fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-m", "base", "-q"]);
    fs::write(root.join("a.txt"), b"one\n").expect("write stash fixture");
    git(root, &["stash", "push", "-q", "-m", "one"]);
}

fn copy_dir(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("create copied directory");
    for entry in fs::read_dir(source).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type().expect("entry file type").is_dir() {
            copy_dir(&source_path, &target_path);
        } else {
            fs::copy(&source_path, &target_path).expect("copy fixture file");
        }
    }
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

#[test]
fn stash_clear_matches_upstream_git() {
    let root = unique_temp_dir("stash-clear");
    let template = root.join("template");
    let upstream = root.join("upstream");
    let actual = root.join("actual");
    let result = (|| {
        prepare_stash_repo(&template);
        copy_dir(&template, &upstream);
        copy_dir(&template, &actual);

        let args = ["stash", "clear"];
        let expected = run_output("git", &upstream, &args);
        let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
        assert_same_output(actual_output, expected, &args);

        for args in [
            vec!["stash", "list"],
            vec!["status", "--show-stash"],
            vec!["show-ref", "--exists", "refs/stash"],
        ] {
            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_clear_empty_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-clear-empty-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);

        for args in [
            vec!["stash", "clear"],
            vec!["stash", "clear", "extra"],
            vec!["stash", "clear", "--bogus"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_drop_matches_upstream_git() {
    let root = unique_temp_dir("stash-drop");
    let template = root.join("template");
    let result = (|| {
        prepare_stash_repo(&template);

        for (name, args) in [
            ("default", vec!["stash", "drop"]),
            ("explicit", vec!["stash", "drop", "stash@{1}"]),
            ("full-ref", vec!["stash", "drop", "refs/stash@{0}"]),
            ("quiet", vec!["stash", "drop", "--quiet", "stash@{0}"]),
            ("no-quiet", vec!["stash", "drop", "--no-quiet", "stash@{0}"]),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            for check_args in [
                vec!["stash", "list"],
                vec!["status", "--show-stash"],
                vec!["show-ref", "--exists", "refs/stash"],
            ] {
                let expected = run_output("git", &upstream, &check_args);
                let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                assert_same_output(actual_output, expected, &check_args);
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn stash_drop_last_and_errors_match_upstream_git() {
    let root = unique_temp_dir("stash-drop-last-errors");
    let template = root.join("template");
    let result = (|| {
        prepare_single_stash_repo(&template);

        for (name, args) in [
            ("last", vec!["stash", "drop"]),
            ("empty", vec!["stash", "drop", "stash@{99}"]),
            ("invalid", vec!["stash", "drop", "bad"]),
            ("unknown-option", vec!["stash", "drop", "--bogus"]),
            ("too-many", vec!["stash", "drop", "stash@{0}", "extra"]),
        ] {
            let upstream = root.join(format!("{name}-upstream"));
            let actual = root.join(format!("{name}-actual"));
            copy_dir(&template, &upstream);
            copy_dir(&template, &actual);

            let expected = run_output("git", &upstream, &args);
            let actual_output = run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &args);
            assert_same_output(actual_output, expected, &args);

            if name == "last" {
                for check_args in [
                    vec!["stash", "list"],
                    vec!["status", "--show-stash"],
                    vec!["show-ref", "--exists", "refs/stash"],
                ] {
                    let expected = run_output("git", &upstream, &check_args);
                    let actual_output =
                        run_output(env!("CARGO_BIN_EXE_git-rs"), &actual, &check_args);
                    assert_same_output(actual_output, expected, &check_args);
                }
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
