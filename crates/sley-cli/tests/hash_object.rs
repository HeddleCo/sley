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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_with_stdin(program, cwd, args, &[])
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let output = run_output_with_stdin(program, cwd, args, stdin);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
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

fn run_output_with_env_and_stdin(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
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

fn git_rs(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin)
}

fn git(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin("git", cwd, args, stdin)
}

#[test]
fn hash_object_git_object_directory_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run("git", &expected, &["init", "-q"]);
        run("git", &actual, &["init", "-q"]);
        for repo in [&expected, &actual] {
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom object dir");
        }
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        let write_args = ["hash-object", "-w", "--stdin"];
        let stdin = b"custom object\n";
        let expected_write =
            run_output_with_env_and_stdin("git", &expected, &write_args, &envs, stdin);
        let actual_write = run_output_with_env_and_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &write_args,
            &envs,
            stdin,
        );
        assert_same_output(actual_write, expected_write, &write_args);

        let oid = String::from_utf8(
            run_output_with_env_and_stdin("git", &expected, &write_args, &envs, stdin).stdout,
        )
        .expect("oid is utf8")
        .trim()
        .to_string();
        let cat_args = ["cat-file", "-p", oid.as_str()];
        let expected_cat = run_output_with_env_and_stdin("git", &expected, &cat_args, &envs, &[]);
        let actual_cat = run_output_with_env_and_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &cat_args,
            &envs,
            &[],
        );
        assert_same_output(actual_cat, expected_cat, &cat_args);

        let (fanout, name) = oid.split_at(2);
        assert!(
            expected
                .join("custom-objects")
                .join(fanout)
                .join(name)
                .exists()
        );
        assert!(
            actual
                .join("custom-objects")
                .join(fanout)
                .join(name)
                .exists()
        );
        assert!(
            !actual
                .join(".git")
                .join("objects")
                .join(fanout)
                .join(name)
                .exists()
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_multiple_inputs_match_upstream_git() {
    let root = unique_temp_dir("hash-object-multiple-inputs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run("git", &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");
        fs::write(root.join("--stdin"), b"dash stdin\n").expect("write option-like path");
        fs::write(root.join("--path"), b"dash path\n").expect("write path-like path");

        let stdin = b"stdin\n";
        for args in [
            vec!["hash-object", "one.txt", "two.txt"],
            vec!["hash-object", "-tblob", "one.txt"],
            vec!["hash-object", "--stdin", "one.txt", "two.txt"],
            vec!["hash-object", "-tblob", "--stdin"],
            vec!["hash-object", "-w", "--stdin", "one.txt", "two.txt"],
            vec!["hash-object", "-w", "-tblob", "one.txt"],
            vec!["hash-object", "--", "--stdin", "--path"],
            vec!["hash-object", "--stdin", "--", "--stdin", "one.txt"],
            vec!["hash-object", "-w", "--", "--stdin"],
            vec!["hash-object", "--"],
        ] {
            let expected = git(&root, &args, stdin);
            let actual = git_rs(&root, &args, stdin);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_sha256_repo_default_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-sha256-default");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run("git", &expected, &["init", "-q", "--object-format=sha256"]);
        run("git", &actual, &["init", "-q", "--object-format=sha256"]);
        for repo in [&expected, &actual] {
            fs::write(repo.join("one.txt"), b"one\n").expect("write one");
            fs::write(repo.join("two.txt"), b"two\n").expect("write two");
            fs::write(repo.join("paths.txt"), b"one.txt\ntwo.txt\n").expect("write path list");
        }

        for (args, stdin) in [
            (vec!["hash-object", "--stdin"], b"stdin\n".as_slice()),
            (vec!["hash-object", "one.txt"], b"".as_slice()),
            (
                vec!["hash-object", "--stdin", "one.txt"],
                b"stdin\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\ntwo.txt\n".as_slice(),
            ),
        ] {
            let expected_output = run_output_with_stdin("git", &expected, &args, stdin);
            let actual_output =
                run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, stdin);
            assert_same_output(actual_output, expected_output, &args);
            assert!(
                String::from_utf8_lossy(&git_rs(&actual, &args, stdin))
                    .lines()
                    .all(|line| line.len() == 64),
                "expected SHA-256 object ids for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_filter_path_and_option_errors_match_upstream_git() {
    let root = unique_temp_dir("hash-object-filter-path");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run("git", &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        let stdin = b"stdin\n";

        for args in [
            vec!["hash-object", "--no-filters", "one.txt"],
            vec!["hash-object", "--filters", "one.txt"],
            vec!["hash-object", "--literally", "one.txt"],
            vec!["hash-object", "--no-literally", "one.txt"],
            vec!["hash-object", "--no-stdin", "one.txt"],
            vec!["hash-object", "--path=virtual.txt", "--stdin"],
            vec!["hash-object", "--path", "virtual.txt", "--stdin"],
            vec!["hash-object", "--no-path", "--stdin"],
            vec![
                "hash-object",
                "--path",
                "virtual.txt",
                "--no-path",
                "--stdin",
            ],
            vec![
                "hash-object",
                "--no-path",
                "--path",
                "virtual.txt",
                "--stdin",
            ],
            vec!["hash-object", "--path=virtual.txt", "--no-path", "--stdin"],
            vec!["hash-object", "--no-filters", "--stdin"],
            vec!["hash-object", "--filters", "--stdin"],
            vec!["hash-object", "--literally", "--stdin"],
            vec!["hash-object", "--no-literally", "--stdin"],
            vec!["hash-object", "--no-stdin"],
            vec!["hash-object", "-t"],
            vec!["hash-object", "--stdin=value", "--stdin"],
            vec!["hash-object", "--no-stdin=value", "--stdin"],
            vec!["hash-object", "--stdin-paths=value", "--stdin"],
            vec!["hash-object", "--no-stdin-paths=value", "--stdin"],
            vec!["hash-object", "--filters=value", "--stdin"],
            vec!["hash-object", "--no-filters=value", "--stdin"],
            vec!["hash-object", "--literally=value", "--stdin"],
            vec!["hash-object", "--no-literally=value", "--stdin"],
            vec!["hash-object", "--no-path=value", "--stdin"],
        ] {
            let expected = run_output_with_stdin("git", &root, &args, stdin);
            let actual = run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_stdin_paths_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-stdin-paths");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run("git", &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");

        for (args, stdin) in [
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\ntwo.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths", "--no-filters"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths", "--no-stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--no-stdin-paths", "--stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--no-stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (vec!["hash-object", "--stdin-paths"], b"".as_slice()),
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\nmissing\n".as_slice(),
            ),
        ] {
            let expected = run_output_with_stdin("git", &root, &args, stdin);
            let actual = run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
