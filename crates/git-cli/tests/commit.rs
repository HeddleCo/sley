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

fn run_output_with_identity_and_editor(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .env("GIT_EDITOR", "true")
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

fn create_initial_commit(program: &str, root: &Path) {
    prepare_commit_repo(root);
    let output = run_output_with_identity(program, root, &["commit", "-m", "initial"]);
    assert!(
        output.status.success(),
        "{program} initial commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn remove_message_fixtures(root: &Path) {
    for name in [
        "message-empty.txt",
        "message-lf.txt",
        "message-no-lf.txt",
        "message-signed.txt",
        "message-whitespace.txt",
    ] {
        fs::remove_file(root.join(name)).expect("remove message fixture");
    }
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
fn commit_clean_index_requires_allow_empty_like_upstream_git() {
    let root = unique_temp_dir("commit-clean-index");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("clean", vec!["commit", "-m", "second"]),
            (
                "allow-then-disallow",
                vec![
                    "commit",
                    "--allow-empty",
                    "--no-allow-empty",
                    "-m",
                    "second",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            create_initial_commit("git", &expected_root);
            create_initial_commit(env!("CARGO_BIN_EXE_git-rs"), &actual_root);
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);

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
        fs::write(root.join("message-lf.txt"), b"file two\n").expect("write lf message");
        for args in [vec!["commit", "-m"], vec!["commit", "-m", "one", "-m"]] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
        let args = vec!["commit", "--cleanup=bad", "-m", "subject"];
        let expected = run_output("git", &root, &args);
        let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
        assert_same_output(actual, expected, &args);
        for args in [
            vec!["commit", "--template"],
            vec!["commit", "--no-template=value", "-m", "subject"],
            vec!["commit", "--no-file=value", "-m", "subject"],
            vec!["commit", "--no-cleanup=value", "-m", "subject"],
            vec!["commit", "--all=value", "-m", "subject"],
            vec!["commit", "--no-all=value", "-m", "subject"],
            vec!["commit", "-C"],
            vec!["commit", "--reuse-message"],
            vec!["commit", "-C", "HEAD", "-m", "subject"],
            vec!["commit", "-C", "HEAD", "-F", "message-lf.txt"],
            vec!["commit", "--reuse-message=missing"],
            vec!["commit", "-c"],
            vec!["commit", "--reedit-message"],
            vec!["commit", "-c", "HEAD", "-m", "subject"],
            vec!["commit", "-c", "HEAD", "-F", "message-lf.txt"],
            vec!["commit", "--reedit-message=missing"],
            vec!["commit", "--no-reuse-message=value", "-m", "subject"],
            vec!["commit", "--no-reedit-message=value", "-m", "subject"],
            vec!["commit", "--fixup"],
            vec!["commit", "--fixup=missing"],
            vec!["commit", "--fixup=amend:missing"],
            vec!["commit", "--fixup=reword:missing"],
            vec!["commit", "--fixup=bad:HEAD"],
            vec!["commit", "-C", "HEAD", "--fixup", "HEAD"],
            vec!["commit", "-c", "HEAD", "--fixup", "HEAD"],
            vec!["commit", "--fixup", "HEAD", "-F", "message-lf.txt"],
            vec!["commit", "--squash"],
            vec!["commit", "--squash=missing"],
            vec!["commit", "--squash", "HEAD", "--fixup", "HEAD"],
            vec!["commit", "--no-message=value", "-m", "subject"],
            vec!["commit", "--no-fixup=value", "-m", "subject"],
            vec!["commit", "--no-squash=value", "-m", "subject"],
            vec!["commit", "-m", "subject", "--trailer"],
            vec!["commit", "-m", "subject", "--no-trailer=value"],
            vec!["commit", "--edit=value", "-m", "subject"],
            vec!["commit", "--no-edit=value", "-m", "subject"],
            vec!["commit", "--branch=value", "-m", "subject"],
            vec!["commit", "--no-branch=value", "-m", "subject"],
            vec!["commit", "--no-author=value", "-m", "subject"],
            vec!["commit", "--no-date=value", "-m", "subject"],
            vec!["commit", "--signoff=value", "-m", "subject"],
            vec!["commit", "--no-signoff=value", "-m", "subject"],
            vec!["commit", "--quiet=value", "-m", "subject"],
            vec!["commit", "--no-quiet=value", "-m", "subject"],
            vec!["commit", "--allow-empty=value", "-m", "subject"],
            vec!["commit", "--no-allow-empty=value", "-m", "subject"],
            vec!["commit", "--allow-empty-message=value", "-m", "subject"],
            vec!["commit", "--no-allow-empty-message=value", "-m", "subject"],
            vec!["commit", "--no-verify=value", "-m", "subject"],
            vec!["commit", "--verify=value", "-m", "subject"],
            vec!["commit", "--no-gpg-sign=value", "-m", "subject"],
            vec!["commit", "--post-rewrite=value", "-m", "subject"],
            vec!["commit", "--no-post-rewrite=value", "-m", "subject"],
            vec!["commit", "--status=value", "-m", "subject"],
            vec!["commit", "--no-status=value", "-m", "subject"],
            vec!["commit", "--verbose=value", "-m", "subject"],
            vec!["commit", "--no-verbose=value", "-m", "subject"],
            vec!["commit", "--untracked-files=bad", "-m", "subject"],
            vec!["commit", "-ubad", "-m", "subject"],
            vec!["commit", "--no-untracked-files=value", "-m", "subject"],
            vec!["commit", "--include", "-m", "subject"],
            vec!["commit", "--only", "-m", "subject"],
            vec!["commit", "-i", "-m", "subject"],
            vec!["commit", "-o", "-m", "subject"],
            vec!["commit", "--include=value", "-m", "subject"],
            vec!["commit", "--only=value", "-m", "subject"],
            vec!["commit", "--no-include=value", "-m", "subject"],
            vec!["commit", "--no-only=value", "-m", "subject"],
            vec!["commit", "--reset-author", "-m", "subject"],
            vec!["commit", "--reset-author=value", "-m", "subject"],
            vec!["commit", "--no-reset-author=value", "-m", "subject"],
            vec!["commit", "--amend", "-m", "subject"],
            vec!["commit", "--amend=value", "-m", "subject"],
            vec!["commit", "--no-amend=value", "-m", "subject"],
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
            ("no-file", vec!["commit", "--no-file", "-m", "subject"]),
            (
                "file-no-file",
                vec!["commit", "--file", "message-no-lf.txt", "--no-file"],
            ),
            (
                "attached-and-long-message",
                vec!["commit", "-mone", "--message", "two"],
            ),
            (
                "message-reset",
                vec![
                    "commit",
                    "--message",
                    "ignored",
                    "--no-message",
                    "-m",
                    "subject",
                ],
            ),
            (
                "message-no-gpg-sign",
                vec!["commit", "--message=subject", "--no-gpg-sign"],
            ),
            (
                "reuse-reset",
                vec![
                    "commit",
                    "--reuse-message=HEAD",
                    "--no-reuse-message",
                    "-m",
                    "subject",
                ],
            ),
            (
                "reedit-reset",
                vec![
                    "commit",
                    "--reedit-message=HEAD",
                    "--no-reedit-message",
                    "-m",
                    "subject",
                ],
            ),
            (
                "no-reset-author-noop",
                vec!["commit", "--no-reset-author", "-m", "subject"],
            ),
            (
                "no-amend-reset",
                vec!["commit", "--amend", "--no-amend", "-m", "subject"],
            ),
            (
                "fixup-reset",
                vec!["commit", "--fixup", "HEAD", "--no-fixup", "-m", "subject"],
            ),
            (
                "squash-reset",
                vec!["commit", "--squash", "HEAD", "--no-squash", "-m", "subject"],
            ),
            (
                "trailer-equals",
                vec!["commit", "-m", "subject", "--trailer", "Acked-by=Alice"],
            ),
            (
                "trailer-colon",
                vec!["commit", "-m", "subject", "--trailer=Acked-by:Alice"],
            ),
            (
                "trailer-multiple",
                vec![
                    "commit",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                    "--trailer",
                    "Reviewed-by=Bob",
                ],
            ),
            (
                "trailer-body",
                vec![
                    "commit",
                    "-m",
                    "subject",
                    "-m",
                    "body",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "trailer-clear",
                vec![
                    "commit",
                    "-m",
                    "subject",
                    "--trailer",
                    "Acked-by=Alice",
                    "--no-trailer",
                ],
            ),
            (
                "trailer-clear-before",
                vec![
                    "commit",
                    "-m",
                    "subject",
                    "--no-trailer",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "signoff-trailer",
                vec![
                    "commit",
                    "-m",
                    "subject",
                    "-s",
                    "--trailer",
                    "Acked-by=Alice",
                ],
            ),
            (
                "all-no-all",
                vec!["commit", "--all", "--no-all", "-m", "subject"],
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
                "post-rewrite",
                vec!["commit", "--post-rewrite", "-m", "subject"],
            ),
            (
                "no-post-rewrite",
                vec!["commit", "--no-post-rewrite", "-m", "subject"],
            ),
            ("status", vec!["commit", "--status", "-m", "subject"]),
            ("no-status", vec!["commit", "--no-status", "-m", "subject"]),
            ("verbose", vec!["commit", "--verbose", "-m", "subject"]),
            (
                "no-verbose",
                vec!["commit", "--no-verbose", "-m", "subject"],
            ),
            (
                "untracked-files",
                vec!["commit", "--untracked-files", "-m", "subject"],
            ),
            (
                "untracked-files-no",
                vec!["commit", "--untracked-files=no", "-m", "subject"],
            ),
            (
                "untracked-files-normal",
                vec!["commit", "--untracked-files=normal", "-m", "subject"],
            ),
            (
                "untracked-files-all",
                vec!["commit", "--untracked-files=all", "-m", "subject"],
            ),
            ("untracked-short", vec!["commit", "-u", "-m", "subject"]),
            (
                "untracked-short-no",
                vec!["commit", "-uno", "-m", "subject"],
            ),
            (
                "untracked-short-normal",
                vec!["commit", "-unormal", "-m", "subject"],
            ),
            (
                "untracked-short-all",
                vec!["commit", "-uall", "-m", "subject"],
            ),
            (
                "no-untracked-files",
                vec!["commit", "--no-untracked-files", "-m", "subject"],
            ),
            (
                "no-include",
                vec!["commit", "--no-include", "-m", "subject"],
            ),
            ("no-only", vec!["commit", "--no-only", "-m", "subject"]),
            (
                "include-reset",
                vec!["commit", "--include", "--no-include", "-m", "subject"],
            ),
            (
                "only-reset",
                vec!["commit", "--only", "--no-only", "-m", "subject"],
            ),
            ("no-edit", vec!["commit", "--no-edit", "-m", "subject"]),
            ("branch", vec!["commit", "--branch", "-m", "subject"]),
            ("no-branch", vec!["commit", "--no-branch", "-m", "subject"]),
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
            (
                "cleanup-strip",
                vec![
                    "commit",
                    "--cleanup=strip",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "cleanup-whitespace",
                vec![
                    "commit",
                    "--cleanup",
                    "whitespace",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "cleanup-verbatim",
                vec![
                    "commit",
                    "--cleanup=verbatim",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "cleanup-default",
                vec![
                    "commit",
                    "--cleanup=default",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "cleanup-scissors",
                vec![
                    "commit",
                    "--cleanup=scissors",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "cleanup-reset",
                vec![
                    "commit",
                    "--cleanup=strip",
                    "--no-cleanup",
                    "-m",
                    "subject\n\n# comment\n body  ",
                ],
            ),
            (
                "template-long",
                vec!["commit", "--template", "message-lf.txt", "-m", "subject"],
            ),
            (
                "template-equals",
                vec!["commit", "--template=message-lf.txt", "-m", "subject"],
            ),
            (
                "template-empty-equals",
                vec!["commit", "--template=", "-m", "subject"],
            ),
            (
                "no-template",
                vec![
                    "commit",
                    "--template=message-lf.txt",
                    "--no-template",
                    "-m",
                    "subject",
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

        let expected_root = root.join("trailer-only-expected");
        let actual_root = root.join("trailer-only-actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_commit_repo(&expected_root);
        prepare_commit_repo(&actual_root);
        let args = ["commit", "--trailer", "Acked-by=Alice"];
        let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &args);
        assert!(
            actual.status.success(),
            "git-rs {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head("git", &actual_root),
            cat_head("git", &expected_root),
            "committed object differed for trailer-only"
        );

        for (name, args) in [
            ("edit-short", vec!["commit", "-e", "-m", "subject"]),
            ("edit-long", vec!["commit", "--edit", "-m", "subject"]),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);

            let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual_root,
                &args,
            );
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_all_stages_tracked_changes_like_upstream_git_objects() {
    let root = unique_temp_dir("commit-all-tracked");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args, deleted) in [
            ("all-modified", vec!["commit", "-a", "-m", "second"], false),
            ("all-long", vec!["commit", "--all", "-m", "second"], false),
            ("all-attached-message", vec!["commit", "-amsecond"], false),
            ("all-deleted", vec!["commit", "-a", "-m", "second"], true),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            create_initial_commit("git", &expected_root);
            create_initial_commit(env!("CARGO_BIN_EXE_git-rs"), &actual_root);
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);
            if deleted {
                fs::remove_file(expected_root.join("tracked.txt"))
                    .expect("delete expected tracked");
                fs::remove_file(actual_root.join("tracked.txt")).expect("delete actual tracked");
            } else {
                fs::write(expected_root.join("tracked.txt"), b"changed\n")
                    .expect("modify expected tracked");
                fs::write(actual_root.join("tracked.txt"), b"changed\n")
                    .expect("modify actual tracked");
            }
            fs::write(expected_root.join("untracked.txt"), b"untracked\n")
                .expect("write expected untracked");
            fs::write(actual_root.join("untracked.txt"), b"untracked\n")
                .expect("write actual untracked");

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
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_reuse_message_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-reuse-message");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("reuse-short", vec!["commit", "-C", "HEAD"]),
            ("reuse-attached", vec!["commit", "-CHEAD"]),
            ("reuse-long", vec!["commit", "--reuse-message", "HEAD"]),
            ("reuse-equals", vec!["commit", "--reuse-message=HEAD"]),
            (
                "reuse-author-override",
                vec![
                    "commit",
                    "-C",
                    "HEAD",
                    "--author=Override User <override@example.invalid>",
                ],
            ),
            (
                "reuse-date-override",
                vec!["commit", "-C", "HEAD", "--date=@456 +0230"],
            ),
            (
                "reuse-reset-author",
                vec!["commit", "-C", "HEAD", "--reset-author"],
            ),
            (
                "reuse-reset-author-cancelled",
                vec![
                    "commit",
                    "-C",
                    "HEAD",
                    "--reset-author",
                    "--no-reset-author",
                ],
            ),
            (
                "reuse-reset-author-restored",
                vec![
                    "commit",
                    "-C",
                    "HEAD",
                    "--no-reset-author",
                    "--reset-author",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);
            let initial_args = [
                "commit",
                "--author=Reuse User <reuse@example.invalid>",
                "--date=@123 +0000",
                "-m",
                "reused subject",
                "-m",
                "reused body",
            ];
            let expected_initial = run_output_with_identity("git", &expected_root, &initial_args);
            assert!(
                expected_initial.status.success(),
                "git initial commit failed: {}",
                String::from_utf8_lossy(&expected_initial.stderr)
            );
            let actual_initial =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &initial_args);
            assert!(
                actual_initial.status.success(),
                "git-rs initial commit failed: {}",
                String::from_utf8_lossy(&actual_initial.stderr)
            );
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);
            fs::write(expected_root.join("tracked.txt"), b"changed\n")
                .expect("modify expected tracked");
            fs::write(actual_root.join("tracked.txt"), b"changed\n")
                .expect("modify actual tracked");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);

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
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_reedit_message_matches_upstream_git_objects_when_editor_is_noop() {
    let root = unique_temp_dir("commit-reedit-message");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("reedit-short", vec!["commit", "-c", "HEAD"]),
            ("reedit-attached", vec!["commit", "-cHEAD"]),
            ("reedit-long", vec!["commit", "--reedit-message", "HEAD"]),
            ("reedit-equals", vec!["commit", "--reedit-message=HEAD"]),
            (
                "reedit-author-date-override",
                vec![
                    "commit",
                    "-c",
                    "HEAD",
                    "--author=Override User <override@example.invalid>",
                    "--date=@456 +0230",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);
            let initial_args = [
                "commit",
                "--author=Reuse User <reuse@example.invalid>",
                "--date=@123 +0000",
                "-m",
                "reused subject",
                "-m",
                "reused body",
            ];
            let expected_initial = run_output_with_identity("git", &expected_root, &initial_args);
            assert!(
                expected_initial.status.success(),
                "git initial commit failed: {}",
                String::from_utf8_lossy(&expected_initial.stderr)
            );
            let actual_initial =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &initial_args);
            assert!(
                actual_initial.status.success(),
                "git-rs initial commit failed: {}",
                String::from_utf8_lossy(&actual_initial.stderr)
            );
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);
            fs::write(expected_root.join("tracked.txt"), b"changed\n")
                .expect("modify expected tracked");
            fs::write(actual_root.join("tracked.txt"), b"changed\n")
                .expect("modify actual tracked");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);

            let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual_root,
                &args,
            );
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_amend_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-amend");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("amend-message", vec!["commit", "--amend", "-m", "amended"]),
            ("amend-no-edit", vec!["commit", "--amend", "--no-edit"]),
            (
                "amend-reset-author",
                vec!["commit", "--amend", "--reset-author", "-m", "amended"],
            ),
            (
                "amend-reset-author-cancelled",
                vec![
                    "commit",
                    "--amend",
                    "--reset-author",
                    "--no-reset-author",
                    "-m",
                    "amended",
                ],
            ),
            (
                "amend-author-date-override",
                vec![
                    "commit",
                    "--amend",
                    "--author=Override User <override@example.invalid>",
                    "--date=@456 +0230",
                    "-m",
                    "amended",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            create_initial_commit("git", &expected_root);
            create_initial_commit(env!("CARGO_BIN_EXE_git-rs"), &actual_root);
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);

            fs::write(expected_root.join("tracked.txt"), b"old\n").expect("modify expected old");
            fs::write(actual_root.join("tracked.txt"), b"old\n").expect("modify actual old");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);
            let old_args = [
                "commit",
                "--author=Reuse User <reuse@example.invalid>",
                "--date=@123 +0000",
                "-m",
                "old subject",
                "-m",
                "old body",
            ];
            let expected_old = run_output_with_identity("git", &expected_root, &old_args);
            assert!(
                expected_old.status.success(),
                "git old commit failed: {}",
                String::from_utf8_lossy(&expected_old.stderr)
            );
            let actual_old =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &old_args);
            assert!(
                actual_old.status.success(),
                "git-rs old commit failed: {}",
                String::from_utf8_lossy(&actual_old.stderr)
            );

            fs::write(expected_root.join("tracked.txt"), b"amended\n")
                .expect("modify expected amend");
            fs::write(actual_root.join("tracked.txt"), b"amended\n").expect("modify actual amend");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);

            let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual_root,
                &args,
            );
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_fixup_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-fixup");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("fixup-long", vec!["commit", "--fixup", "HEAD"]),
            ("fixup-equals", vec!["commit", "--fixup=HEAD"]),
            (
                "fixup-message",
                vec!["commit", "--fixup", "HEAD", "-m", "body"],
            ),
            ("fixup-amend", vec!["commit", "--fixup=amend:HEAD"]),
            ("fixup-reword", vec!["commit", "--fixup=reword:HEAD"]),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);
            let initial_args = ["commit", "-m", "initial subject", "-m", "initial body"];
            let expected_initial = run_output_with_identity("git", &expected_root, &initial_args);
            assert!(
                expected_initial.status.success(),
                "git initial commit failed: {}",
                String::from_utf8_lossy(&expected_initial.stderr)
            );
            let actual_initial =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &initial_args);
            assert!(
                actual_initial.status.success(),
                "git-rs initial commit failed: {}",
                String::from_utf8_lossy(&actual_initial.stderr)
            );
            fs::write(expected_root.join("tracked.txt"), b"changed\n")
                .expect("modify expected tracked");
            fs::write(actual_root.join("tracked.txt"), b"changed\n")
                .expect("modify actual tracked");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);

            let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual_root,
                &args,
            );
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_squash_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-squash");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            ("squash-long", vec!["commit", "--squash", "HEAD"]),
            (
                "squash-equals",
                vec!["commit", "--squash=HEAD", "-m", "body"],
            ),
            (
                "squash-message",
                vec!["commit", "--squash", "HEAD", "-m", "body"],
            ),
            (
                "squash-file",
                vec!["commit", "--squash", "HEAD", "-F", "message-lf.txt"],
            ),
            (
                "squash-reuse-body",
                vec!["commit", "--squash", "HEAD", "-C", "HEAD"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);
            let initial_args = ["commit", "-m", "initial subject", "-m", "initial body"];
            let expected_initial = run_output_with_identity("git", &expected_root, &initial_args);
            assert!(
                expected_initial.status.success(),
                "git initial commit failed: {}",
                String::from_utf8_lossy(&expected_initial.stderr)
            );
            let actual_initial =
                run_output_with_identity(env!("CARGO_BIN_EXE_git-rs"), &actual_root, &initial_args);
            assert!(
                actual_initial.status.success(),
                "git-rs initial commit failed: {}",
                String::from_utf8_lossy(&actual_initial.stderr)
            );
            fs::write(expected_root.join("tracked.txt"), b"changed\n")
                .expect("modify expected tracked");
            fs::write(actual_root.join("tracked.txt"), b"changed\n")
                .expect("modify actual tracked");
            run_success("git", &expected_root, &["add", "tracked.txt"]);
            run_success("git", &actual_root, &["add", "tracked.txt"]);

            let expected = run_output_with_identity_and_editor("git", &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity_and_editor(
                env!("CARGO_BIN_EXE_git-rs"),
                &actual_root,
                &args,
            );
            assert!(
                actual.status.success(),
                "git-rs {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_allow_empty_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-allow-empty");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            (
                "allow-empty",
                vec!["commit", "--allow-empty", "-m", "second"],
            ),
            (
                "disallow-then-allow-empty",
                vec![
                    "commit",
                    "--no-allow-empty",
                    "--allow-empty",
                    "-m",
                    "second",
                ],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            create_initial_commit("git", &expected_root);
            create_initial_commit(env!("CARGO_BIN_EXE_git-rs"), &actual_root);
            remove_message_fixtures(&expected_root);
            remove_message_fixtures(&actual_root);

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
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn commit_author_and_date_options_match_upstream_git_objects() {
    let root = unique_temp_dir("commit-author-date");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (name, args) in [
            (
                "author-long",
                vec![
                    "commit",
                    "--author",
                    "Other User <other@example.invalid>",
                    "-m",
                    "subject",
                ],
            ),
            (
                "author-equals",
                vec![
                    "commit",
                    "--author=Other User <other@example.invalid>",
                    "-m",
                    "subject",
                ],
            ),
            (
                "date-long",
                vec!["commit", "--date", "@123 +0230", "-m", "subject"],
            ),
            (
                "date-equals",
                vec!["commit", "--date=@123456 -0700", "-m", "subject"],
            ),
            (
                "author-and-date",
                vec![
                    "commit",
                    "--author=Other User <other@example.invalid>",
                    "--date=@123 +0230",
                    "-m",
                    "subject",
                ],
            ),
            (
                "author-reset",
                vec![
                    "commit",
                    "--author=Other User <other@example.invalid>",
                    "--no-author",
                    "-m",
                    "subject",
                ],
            ),
            (
                "date-reset",
                vec!["commit", "--date=@123 +0230", "--no-date", "-m", "subject"],
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
            assert_eq!(
                cat_head("git", &actual_root),
                cat_head("git", &expected_root),
                "committed object differed for {args:?}"
            );
        }
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
