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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

fn prepare_commit_repo(root: &Path) {
    run_success("git", root, &["init", "-q"]);
    fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked file");
    run_success("git", root, &["add", "tracked.txt"]);
    fs::write(root.join("message-no-lf.txt"), b"file one").expect("write no-lf message");
    fs::write(root.join("message-lf.txt"), b"file two\n").expect("write lf message");
    fs::write(root.join("message-empty.txt"), b"").expect("write empty message");
    fs::write(root.join("message-whitespace.txt"), b"  \n\t\n").expect("write whitespace message");
    fs::write(
        root.join("message-signed.txt"),
        b"subject\n\nSigned-off-by: Example User <example@example.invalid>\n",
    )
    .expect("write signed message");
}

fn cat_head(program: &str, root: &Path) -> Vec<u8> {
    let output = run_output(program, root, &["cat-file", "-p", "HEAD"]);
    assert!(
        output.status.success(),
        "{program} cat-file failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn commit_empty_message_errors_match_upstream_git() {
    let root = unique_temp_dir("commit-empty-message-errors");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("empty-inline", vec!["commit", "-m", ""]),
            ("whitespace-inline", vec!["commit", "-m", "   "]),
            ("empty-file", vec!["commit", "-F", "message-empty.txt"]),
            (
                "whitespace-file",
                vec!["commit", "--file", "message-whitespace.txt"],
            ),
            ("empty-signoff", vec!["commit", "-m", "", "-s"]),
            (
                "allow-then-disallow",
                vec![
                    "commit",
                    "-m",
                    "",
                    "--allow-empty-message",
                    "--no-allow-empty-message",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);

            let expected = run_output_with_identity("git", &expected_root, &args);
            let actual =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_message_option_errors_match_upstream_git() {
    let root = unique_temp_dir("commit-message-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        run_success("git", &root, &["init", "-q"]);
        for args in [vec!["commit", "-m"], vec!["commit", "-m", "one", "-m"]] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_file_messages_match_upstream_git_objects() {
    let root = unique_temp_dir("commit-file-messages");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("file-no-lf", vec!["commit", "-F", "message-no-lf.txt"]),
            ("attached-file", vec!["commit", "-Fmessage-lf.txt"]),
            (
                "long-file-equals",
                vec!["commit", "--file=message-no-lf.txt"],
            ),
            ("long-file", vec!["commit", "--file", "message-lf.txt"]),
            (
                "attached-and-long-message",
                vec!["commit", "-mone", "--message", "two"],
            ),
            (
                "message-no-gpg-sign",
                vec!["commit", "--message=subject", "--no-gpg-sign"],
            ),
            ("signoff-short", vec!["commit", "-m", "subject", "-s"]),
            ("signoff-long", vec!["commit", "-m", "subject", "--signoff"]),
            (
                "signoff-cancelled",
                vec!["commit", "-m", "subject", "-s", "--no-signoff"],
            ),
            (
                "signoff-restored",
                vec!["commit", "-m", "subject", "--no-signoff", "-s"],
            ),
            (
                "signoff-deduplicated",
                vec!["commit", "-F", "message-signed.txt", "-s"],
            ),
            (
                "quiet-no-verify",
                vec!["commit", "-q", "--no-verify", "-m", "subject"],
            ),
            (
                "quiet-verify",
                vec!["commit", "--quiet", "--verify", "-m", "subject"],
            ),
            (
                "allow-empty-message",
                vec!["commit", "-m", "", "--allow-empty-message"],
            ),
            (
                "allow-empty-file",
                vec!["commit", "-F", "message-empty.txt", "--allow-empty-message"],
            ),
            (
                "disallow-then-allow-empty",
                vec![
                    "commit",
                    "-m",
                    "",
                    "--no-allow-empty-message",
                    "--allow-empty-message",
                ],
            ),
            (
                "allow-empty-signoff",
                vec!["commit", "-m", "", "-s", "--allow-empty-message"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);

            let expected = run_output_with_identity("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &args);
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            if args.iter().any(|arg| *arg == "-q" || *arg == "--quiet") {
                assert_eq!(actual.stdout, expected.stdout, "quiet stdout differed");
                assert_eq!(actual.stderr, expected.stderr, "quiet stderr differed");
            }
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }

        let expected_root = root.join("mixed-message-expected");
        let actual_root = root.join("mixed-message-actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_commit_repo(&expected_root);
        prepare_commit_repo(&actual_root);
        let args = ["commit", "-F", "message-no-lf.txt", "-m", "inline"];
        let expected = run_output_with_identity("git", &expected_root, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_tree_argument_errors_match_upstream_git() {
    let root = unique_temp_dir("commit-tree-argument-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        run_success("git", &root, &["init", "-q"]);
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        for args in [
            vec!["commit-tree", empty_tree, "-m"],
            vec!["commit-tree", empty_tree, "-m", "one", "-m"],
            vec!["commit-tree", empty_tree, "-p"],
            vec!["commit-tree", "-m", "message"],
            vec!["commit-tree", empty_tree, empty_tree, "-m", "message"],
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
fn commit_tree_file_messages_match_upstream_git() {
    let root = unique_temp_dir("commit-tree-file-messages");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        run_success("git", &root, &["init", "-q"]);
        fs::write(root.join("message-no-lf.txt"), b"file one").expect("write no-lf message");
        fs::write(root.join("message-lf.txt"), b"file two\n").expect("write lf message");
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        for args in [
            vec!["commit-tree", empty_tree, "-F", "message-no-lf.txt"],
            vec!["commit-tree", empty_tree, "-Fmessage-lf.txt"],
            vec!["commit-tree", empty_tree, "-mattached"],
            vec![
                "commit-tree",
                empty_tree,
                "-F",
                "message-no-lf.txt",
                "-m",
                "inline",
                "-F",
                "message-lf.txt",
            ],
            vec![
                "commit-tree",
                "--no-gpg-sign",
                empty_tree,
                "-F",
                "message-lf.txt",
            ],
            vec!["commit-tree", empty_tree, "-F"],
        ] {
            let expected = run_output_with_identity("git", &root, &args);
            let actual = run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
        let parent =
            run_output_with_identity("git", &root, &["commit-tree", empty_tree, "-mparent"]);
        assert!(
            parent.status.success(),
            "parent commit creation failed: {}",
            String::from_utf8_lossy(&parent.stderr)
        );
        let parent = String::from_utf8(parent.stdout)
            .expect("parent oid utf8")
            .trim()
            .to_string();
        let args = [
            "commit-tree".to_string(),
            empty_tree.to_string(),
            format!("-p{parent}"),
            "-mchild".to_string(),
        ];
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let expected = run_output_with_identity("git", &root, &args);
        let actual = run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
