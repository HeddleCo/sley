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
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        stdin,
    );

    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_output_with_env(program: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
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

#[test]
fn cat_file_usage_and_option_errors_exit_like_upstream_git() {
    let root = unique_temp_dir("cat-file-usage");
    fs::create_dir_all(&root).expect("create temp root");
    {
        git(&root, &["init", "-q"]);
        for args in [
            vec!["cat-file", "-e", "--batch"],
            vec!["cat-file", "-p", "--batch-check"],
            vec!["cat-file", "-t", "-s", "HEAD"],
            vec!["cat-file", "-e"],
            vec!["cat-file", "--batch", "HEAD"],
            vec!["cat-file", "--path=x", "--batch"],
            vec!["cat-file", "-e", "HEAD", "extra"],
            vec!["cat-file", "--batch-all-objects", "-e"],
            vec!["cat-file", "-z"],
            vec!["cat-file", "--textconv=value", "HEAD"],
        ] {
            let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, b"");
            let actual = run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, b"");
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "sley exit status differed for {args:?}\nexpected stderr:\n{}\nactual stderr:\n{}",
                String::from_utf8_lossy(&expected.stderr),
                String::from_utf8_lossy(&actual.stderr)
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cat_file_reads_alternate_object_directories_like_upstream_git() {
    let root = unique_temp_dir("cat-file-alternates");
    let expected = root.join("expected");
    let actual = root.join("actual");
    let expected_alt = root.join("expected-alt");
    let actual_alt = root.join("actual-alt");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for repo in [&expected, &actual, &expected_alt, &actual_alt] {
            fs::create_dir_all(repo).expect("create repo dir");
            git(repo, &["init", "-q"]);
        }
        fs::write(expected_alt.join("blob.txt"), b"alternate blob\n").expect("write expected blob");
        fs::write(actual_alt.join("blob.txt"), b"alternate blob\n").expect("write actual blob");
        let oid = String::from_utf8(git(&expected_alt, &["hash-object", "-w", "blob.txt"]))
            .expect("oid is utf8")
            .trim()
            .to_string();
        git(&actual_alt, &["hash-object", "-w", "blob.txt"]);

        fs::create_dir_all(expected.join(".git").join("objects").join("info"))
            .expect("create expected alternates dir");
        fs::create_dir_all(actual.join(".git").join("objects").join("info"))
            .expect("create actual alternates dir");
        fs::write(
            expected
                .join(".git")
                .join("objects")
                .join("info")
                .join("alternates"),
            b"../../../expected-alt/.git/objects\n",
        )
        .expect("write expected alternates");
        fs::write(
            actual
                .join(".git")
                .join("objects")
                .join("info")
                .join("alternates"),
            b"../../../actual-alt/.git/objects\n",
        )
        .expect("write actual alternates");

        let args = ["cat-file", "-p", oid.as_str()];
        assert_same_output(
            run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &actual, &args, &[]),
            run_output_with_stdin(sley_testkit::oracle_git(), &expected, &args, &[]),
            &args,
        );

        fs::remove_file(
            expected
                .join(".git")
                .join("objects")
                .join("info")
                .join("alternates"),
        )
        .expect("remove expected alternates");
        fs::remove_file(
            actual
                .join(".git")
                .join("objects")
                .join("info")
                .join("alternates"),
        )
        .expect("remove actual alternates");
        let expected_alt_objects = expected_alt.join(".git").join("objects");
        let actual_alt_objects = actual_alt.join(".git").join("objects");
        let expected_alt_objects = expected_alt_objects.to_string_lossy().into_owned();
        let actual_alt_objects = actual_alt_objects.to_string_lossy().into_owned();
        let expected_env = [(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            expected_alt_objects.as_str(),
        )];
        let actual_env = [(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            actual_alt_objects.as_str(),
        )];
        assert_same_output(
            run_output_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &actual_env),
            run_output_with_env(sley_testkit::oracle_git(), &expected, &args, &expected_env),
            &args,
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cat_file_batch_all_objects_git_object_directory_matches_upstream_git() {
    let root = unique_temp_dir("cat-file-batch-all-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    {
        for repo in [&expected, &actual] {
            git(repo, &["init", "-q"]);
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom objects dir");
            fs::write(repo.join("one.txt"), b"one\n").expect("write one");
            fs::write(repo.join("two.txt"), b"two\n").expect("write two");
            let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
            for path in ["one.txt", "two.txt"] {
                let output = run_output_with_env(sley_testkit::oracle_git(), repo, &["hash-object", "-w", path], &envs);
                assert!(
                    output.status.success(),
                    "hash-object failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        for args in [
            vec!["cat-file", "--batch-check", "--batch-all-objects"],
            vec!["cat-file", "--batch", "--batch-all-objects"],
        ] {
            assert_same_output(
                run_output_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &envs),
                run_output_with_env(sley_testkit::oracle_git(), &expected, &args, &envs),
                &args,
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cat_file_storage_atoms_git_object_directory_match_upstream_git() {
    let root = unique_temp_dir("cat-file-storage-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    {
        for repo in [&expected, &actual] {
            git(repo, &["init", "-q"]);
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom objects dir");
            fs::write(repo.join("one.txt"), b"one\n").expect("write one");
            let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
            let output = run_output_with_env(sley_testkit::oracle_git(), repo, &["hash-object", "-w", "one.txt"], &envs);
            assert!(
                output.status.success(),
                "hash-object failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        let args = [
            "cat-file",
            "--batch-check=%(objectname)|%(objectsize:disk)|%(deltabase)",
            "--batch-all-objects",
        ];
        assert_same_output(
            run_output_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &envs),
            run_output_with_env(sley_testkit::oracle_git(), &expected, &args, &envs),
            &args,
        );
    };
    let _ = fs::remove_dir_all(&root);
}

fn remove_loose_object(root: &Path, oid: &str) {
    let (fanout, file) = oid.split_at(2);
    let _ = fs::remove_file(root.join(".git").join("objects").join(fanout).join(file));
}

fn git_rs(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn git_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(sley_testkit::oracle_git(), cwd, args, stdin)
}

#[test]
fn cat_file_batch_modes_match_upstream_git() {
    let root = unique_temp_dir("cat-file-batch-check");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write fixture");
        git(&root, &["add", "hello.txt"]);
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
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "tag",
                "-a",
                "v1.0",
                "-m",
                "release",
            ],
        );
        let head = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("HEAD oid is utf8")
            .trim()
            .to_string();
        let tree = String::from_utf8(git(&root, &["rev-parse", "HEAD^{tree}"]))
            .expect("tree oid is utf8")
            .trim()
            .to_string();
        for args in [
            vec!["cat-file", "--use-mailmap", "-p", "HEAD"],
            vec!["cat-file", "-p", "--mailmap", "HEAD"],
            vec!["cat-file", "-p", "HEAD", "--no-use-mailmap"],
            vec!["cat-file", "--no-mailmap", "-s", "HEAD"],
            vec![
                "cat-file",
                "--no-use-mailmap",
                "--use-mailmap",
                "-t",
                "HEAD",
            ],
            vec!["cat-file", "-e", "--mailmap", "HEAD"],
        ] {
            let expected = git_stdin(&root, &args, b"");
            let actual = git_rs(&root, &args, b"");
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        for args in [
            vec!["cat-file", "--use-mailmap=value", "-p", "HEAD"],
            vec!["cat-file", "--mailmap=value", "-p", "HEAD"],
            vec!["cat-file", "--no-use-mailmap=value", "-p", "HEAD"],
            vec!["cat-file", "--no-mailmap=value", "-p", "HEAD"],
        ] {
            let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, b"");
            let actual = run_output_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, b"");
            assert_same_output(actual, expected, &args);
        }
        let input = format!("HEAD\n{head}\n{tree}\nrefs/tags/v1.0\nmissing\n\n");
        for mode in ["--batch-check", "--batch"] {
            let expected = git_stdin(&root, &["cat-file", mode], input.as_bytes());
            let actual = git_rs(&root, &["cat-file", mode], input.as_bytes());
            assert_eq!(actual, expected, "sley {mode} output differed");
        }
        for args in [
            vec!["cat-file", "--batch", "--buffer"],
            vec!["cat-file", "--buffer", "--batch"],
            vec!["cat-file", "--batch", "--no-buffer"],
            vec!["cat-file", "--batch-check", "--buffer"],
            vec!["cat-file", "--batch-check", "--no-buffer"],
            vec!["cat-file", "--batch", "--unordered"],
            vec!["cat-file", "--batch", "--no-unordered"],
            vec!["cat-file", "--batch", "--follow-symlinks"],
            vec!["cat-file", "--batch", "--no-follow-symlinks"],
            vec!["cat-file", "--use-mailmap", "--batch-check"],
            vec!["cat-file", "--batch-check", "--mailmap"],
            vec!["cat-file", "--batch", "--no-use-mailmap"],
            vec!["cat-file", "--no-mailmap", "--batch"],
        ] {
            let expected = git_stdin(&root, &args, input.as_bytes());
            let actual = git_rs(&root, &args, input.as_bytes());
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        let nul_input = format!("HEAD\0{head}\0{tree}\0refs/tags/v1.0\0missing\0");
        for args in [
            vec!["cat-file", "--batch-check", "-z"],
            vec!["cat-file", "--batch", "-z"],
            vec!["cat-file", "--batch-check", "-Z"],
            vec!["cat-file", "--batch", "-Z"],
            vec!["cat-file", "-Z", "--batch-check"],
            vec!["cat-file", "--batch", "--buffer", "-Z"],
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(objecttype)",
                "-z",
            ],
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(objecttype)",
                "-Z",
            ],
        ] {
            let expected = git_stdin(&root, &args, nul_input.as_bytes());
            let actual = git_rs(&root, &args, nul_input.as_bytes());
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        let nul_rest_input = format!("HEAD trailing tokens\0{head} raw\0missing extra\0");
        for args in [
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(rest)|%(objecttype)",
                "-Z",
            ],
            vec![
                "cat-file",
                "--batch=%(objectname)|%(rest)|%(objecttype)",
                "-Z",
            ],
        ] {
            let expected = git_stdin(&root, &args, nul_rest_input.as_bytes());
            let actual = git_rs(&root, &args, nul_rest_input.as_bytes());
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        for (args, input) in [
            (
                vec![
                    "cat-file",
                    "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                ],
                input.as_bytes().to_vec(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch=%(objectname) %(objecttype) %(objectsize)",
                ],
                input.as_bytes().to_vec(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch-check=%(objectname)|%(rest)|%(objecttype)",
                ],
                format!("HEAD trailing tokens\n{head} raw\nmissing extra\n").into_bytes(),
            ),
            (
                vec!["cat-file", "--batch=%(objectname)|%(rest)|%(objecttype)"],
                format!("HEAD trailing tokens\n{head} raw\nmissing extra\n").into_bytes(),
            ),
        ] {
            let expected = git_stdin(&root, &args, &input);
            let actual = git_rs(&root, &args, &input);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        for (args, input) in [
            (
                vec![
                    "cat-file",
                    "--batch-check=%(objectname)|%(objectsize:disk)|%(deltabase)|%(rest)",
                ],
                format!("HEAD trailing tokens\n{head} raw\n{tree} tree\nmissing extra\n")
                    .into_bytes(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch=%(objectname)|%(objectsize:disk)|%(deltabase)|%(rest)",
                ],
                format!("HEAD trailing tokens\n{head} raw\n{tree} tree\nmissing extra\n")
                    .into_bytes(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch-command=%(objectname)|%(objecttype)|%(objectsize:disk)|%(deltabase)",
                ],
                format!("info HEAD\ncontents {tree}\ninfo missing\n").into_bytes(),
            ),
        ] {
            let expected = git_stdin(&root, &args, &input);
            let actual = git_rs(&root, &args, &input);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        for args in [
            vec!["cat-file", "--batch-check", "--batch-all-objects"],
            vec!["cat-file", "--batch", "--batch-all-objects"],
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(objecttype)|%(objectsize)|%(rest)",
                "--batch-all-objects",
            ],
            vec!["cat-file", "--batch-check", "--batch-all-objects", "-Z"],
        ] {
            let expected = git_stdin(&root, &args, b"ignored\n");
            let actual = git_rs(&root, &args, b"ignored\n");
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        for (args, input) in [
            (
                vec!["cat-file", "--batch-command"],
                format!("info HEAD\ncontents {head}\ninfo missing\n").into_bytes(),
            ),
            (
                vec!["cat-file", "--batch-command", "--buffer"],
                format!("info HEAD\nflush\ncontents {tree}\nflush\n").into_bytes(),
            ),
            (
                vec!["cat-file", "--batch-command", "-z"],
                format!("info HEAD\0contents {tree}\0info missing\0").into_bytes(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch-command=%(objectname)|%(objecttype)|%(objectsize)",
                ],
                format!("info HEAD\ncontents {tree}\n").into_bytes(),
            ),
            (
                vec!["cat-file", "--batch-command", "-Z"],
                format!("info HEAD\0contents {tree}\0info missing\0").into_bytes(),
            ),
            (
                vec!["cat-file", "--batch-command", "--batch-all-objects"],
                b"ignored\n".to_vec(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch-command=%(objectname)|%(objecttype)|%(objectsize)",
                    "--batch-all-objects",
                ],
                b"ignored\n".to_vec(),
            ),
        ] {
            let expected = git_stdin(&root, &args, &input);
            let actual = git_rs(&root, &args, &input);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
        git(&root, &["gc", "--quiet"]);
        for (args, input) in [
            (
                vec![
                    "cat-file",
                    "--batch-check=%(objectname)|%(objectsize:disk)|%(deltabase)|%(rest)",
                ],
                format!("HEAD trailing tokens\n{head} raw\n{tree} tree\nmissing extra\n")
                    .into_bytes(),
            ),
            (
                vec![
                    "cat-file",
                    "--batch=%(objectname)|%(objectsize:disk)|%(deltabase)|%(rest)",
                ],
                format!("HEAD trailing tokens\n{head} raw\n{tree} tree\nmissing extra\n")
                    .into_bytes(),
            ),
        ] {
            let expected = git_stdin(&root, &args, &input);
            let actual = git_rs(&root, &args, &input);
            assert_eq!(actual, expected, "sley packed output differed for {args:?}");
        }
        for args in [
            vec!["cat-file", "--batch-check", "--batch-all-objects"],
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(objecttype)|%(objectsize)",
                "--batch-all-objects",
            ],
            vec![
                "cat-file",
                "--batch-check=%(objectname)|%(objectsize:disk)|%(deltabase)",
                "--batch-all-objects",
            ],
        ] {
            let expected = git_stdin(&root, &args, b"ignored\n");
            let actual = git_rs(&root, &args, b"ignored\n");
            assert_eq!(actual, expected, "sley packed output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cat_file_batch_storage_atoms_match_upstream_for_delta_pack() {
    let root = unique_temp_dir("cat-file-delta-storage-atoms");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q"]);
        let mut base = Vec::new();
        for idx in 0..4000 {
            writeln!(&mut base, "common payload line {idx:04}").expect("write base fixture");
        }
        let mut changed = b"changed header\n".to_vec();
        changed.extend_from_slice(&base);
        let base_oid =
            String::from_utf8(git_stdin(&root, &["hash-object", "-w", "--stdin"], &base))
                .expect("base oid utf8")
                .trim()
                .to_string();
        let changed_oid = String::from_utf8(git_stdin(
            &root,
            &["hash-object", "-w", "--stdin"],
            &changed,
        ))
        .expect("changed oid utf8")
        .trim()
        .to_string();
        let pack_prefix = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join("pack-delta");
        let pack_prefix = pack_prefix.to_str().expect("pack prefix utf8");
        let pack_input = format!("{base_oid}\n{changed_oid}\n");
        git_stdin(
            &root,
            &["pack-objects", "--delta-base-offset", pack_prefix],
            pack_input.as_bytes(),
        );
        remove_loose_object(&root, &base_oid);
        remove_loose_object(&root, &changed_oid);
        let input = format!("{base_oid}\n{changed_oid}\n");
        let args = [
            "cat-file",
            "--batch-check=%(objectname)|%(objectsize:disk)|%(deltabase)",
        ];
        let expected = git_stdin(&root, &args, input.as_bytes());
        let zero = "0000000000000000000000000000000000000000";
        assert!(
            String::from_utf8_lossy(&expected)
                .lines()
                .any(|line| !line.ends_with(zero)),
            "fixture did not produce a packed delta object"
        );
        let actual = git_rs(&root, &args, input.as_bytes());
        assert_eq!(actual, expected, "sley delta storage atom output differed");
    };
    let _ = fs::remove_dir_all(&root);
}
