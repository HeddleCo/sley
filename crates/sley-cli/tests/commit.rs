use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
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
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked file");
    run_success(sley_testkit::oracle_git(), root, &["add", "tracked.txt"]);
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

#[test]
fn commit_pathspec_pre_commit_sees_selected_worktree_content() {
    let root = unique_temp_dir("commit-pathspec-pre-commit");
    let result = std::panic::catch_unwind(|| {
        fs::create_dir_all(&root).expect("create repo dir");
        let sley = sley_testkit::sley_bin!();
        run_success(sley, &root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked file");
        run_success(sley, &root, &["add", "tracked.txt"]);
        let initial = run_output_with_identity(sley, &root, &["commit", "-m", "initial"]);
        assert!(
            initial.status.success(),
            "initial commit failed: {}",
            String::from_utf8_lossy(&initial.stderr)
        );

        let hooks = root.join(".git/hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");
        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            format!("#!/bin/sh\n\"{sley}\" diff --cached --check\n").as_bytes(),
        )
        .expect("write pre-commit hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&hook).expect("hook metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&hook, permissions).expect("chmod hook");
        }

        fs::write(root.join("tracked.txt"), b"bad \n").expect("write bad whitespace");
        let rejected =
            run_output_with_identity(sley, &root, &["commit", "-m", "bad", "tracked.txt"]);
        assert!(
            !rejected.status.success(),
            "pathspec commit should have failed pre-commit\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        );
        let status = run_output(sley, &root, &["status", "--short"]);
        assert_eq!(status.stdout, b" M tracked.txt\n");

        let accepted = run_output_with_identity(
            sley,
            &root,
            &["commit", "--no-verify", "-m", "bad", "tracked.txt"],
        );
        assert!(
            accepted.status.success(),
            "--no-verify pathspec commit failed: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );
    });
    let _ = fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn pre_commit_hook_sees_prefix_and_command_line_author() {
    let root = unique_temp_dir("commit-hook-prefix-author");
    let result = std::panic::catch_unwind(|| {
        fs::create_dir_all(&root).expect("create repo dir");
        let sley = sley_testkit::sley_bin!();
        run_success(sley, &root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("tracked.txt"), b"tracked\n").expect("write tracked file");
        run_success(sley, &root, &["add", "tracked.txt"]);
        let initial = run_output_with_identity(sley, &root, &["commit", "-m", "initial"]);
        assert!(
            initial.status.success(),
            "initial commit failed: {}",
            String::from_utf8_lossy(&initial.stderr)
        );

        let hooks = root.join(".git/hooks");
        fs::create_dir_all(&hooks).expect("create hooks dir");
        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            b"#!/bin/sh\n\
              echo ok >>actual_hooks\n\
              test \"$GIT_PREFIX\" = success/ &&\n\
              test \"$GIT_AUTHOR_NAME\" = \"New Author\" &&\n\
              test \"$GIT_AUTHOR_EMAIL\" = newauthor@example.com\n",
        )
        .expect("write pre-commit hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&hook).expect("hook metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&hook, permissions).expect("chmod hook");
        }

        fs::write(root.join("tracked.txt"), b"updated\n").expect("write tracked file");
        run_success(sley, &root, &["add", "tracked.txt"]);
        fs::create_dir(root.join("success")).expect("create subdir");
        let committed = run_output_with_identity(
            sley,
            &root.join("success"),
            &[
                "commit",
                "--author=New Author <newauthor@example.com>",
                "-m",
                "hook author",
            ],
        );
        assert!(
            committed.status.success(),
            "commit failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&committed.stdout),
            String::from_utf8_lossy(&committed.stderr)
        );
        assert_eq!(
            fs::read(root.join("actual_hooks")).expect("read actual hooks"),
            b"ok\n"
        );
    });
    let _ = fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
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

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert_same_output(actual, expected, &args);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_clean_index_requires_allow_empty_like_upstream_git() {
    let root = unique_temp_dir("commit-clean-index");
    fs::create_dir_all(&root).expect("create temp root");
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
        create_initial_commit(sley_testkit::oracle_git(), &expected_root);
        create_initial_commit(sley_testkit::sley_bin!(), &actual_root);
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert_same_output(actual, expected, &args);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_message_option_errors_match_upstream_git() {
    let root = unique_temp_dir("commit-message-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join("message-lf.txt"), b"file two\n").expect("write lf message");
        for args in [
            vec!["commit", "-m"],
            vec!["commit", "-m", "one", "-m"],
            vec!["commit", "-t"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
        let args = vec!["commit", "--cleanup=bad", "-m", "subject"];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &root, &args);
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
            vec!["commit", "--dry-run=value", "-m", "subject"],
            vec!["commit", "--no-dry-run=value", "-m", "subject"],
            vec!["commit", "--short=value", "-m", "subject"],
            vec!["commit", "--no-short=value", "-m", "subject"],
            vec!["commit", "--porcelain=value", "-m", "subject"],
            vec!["commit", "--porcelain=v1", "-m", "subject"],
            vec!["commit", "--no-porcelain=value", "-m", "subject"],
            vec!["commit", "--null=value", "-m", "subject"],
            vec!["commit", "--no-null=value", "-m", "subject"],
            vec!["commit", "--long=value", "-m", "subject"],
            vec!["commit", "--no-long=value", "-m", "subject"],
            vec!["commit", "--ahead-behind=value", "-m", "subject"],
            vec!["commit", "--no-ahead-behind=value", "-m", "subject"],
            vec!["commit", "--interactive=value", "-m", "subject"],
            vec!["commit", "--no-interactive=value", "-m", "subject"],
            vec!["commit", "--patch=value", "-m", "subject"],
            vec!["commit", "--no-patch=value", "-m", "subject"],
            vec!["commit", "-U"],
            vec!["commit", "-U", "", "-m", "subject"],
            vec!["commit", "-Ubad", "-m", "subject"],
            vec!["commit", "--unified"],
            vec!["commit", "--unified=", "-m", "subject"],
            vec!["commit", "--unified=bad", "-m", "subject"],
            vec!["commit", "--unified", "bad", "-m", "subject"],
            vec!["commit", "--inter-hunk-context"],
            vec!["commit", "--inter-hunk-context=", "-m", "subject"],
            vec!["commit", "--inter-hunk-context=bad", "-m", "subject"],
            vec!["commit", "--inter-hunk-context", "bad", "-m", "subject"],
            vec!["commit", "-U3", "-m", "subject"],
            vec!["commit", "-U", "3", "-m", "subject"],
            vec!["commit", "--unified=3", "-m", "subject"],
            vec!["commit", "--unified", "3", "-m", "subject"],
            vec!["commit", "--inter-hunk-context=2", "-m", "subject"],
            vec!["commit", "--inter-hunk-context", "2", "-m", "subject"],
            vec!["commit", "--dry-run", "-U3", "--short"],
            vec!["commit", "--verbose=value", "-m", "subject"],
            vec!["commit", "--no-verbose=value", "-m", "subject"],
            vec!["commit", "--untracked-files=bad", "-m", "subject"],
            vec!["commit", "-ubad", "-m", "subject"],
            vec!["commit", "--no-untracked-files=value", "-m", "subject"],
            vec!["commit", "--pathspec-from-file"],
            vec!["commit", "--no-pathspec-from-file=value", "-m", "subject"],
            vec!["commit", "--pathspec-file-nul=value", "-m", "subject"],
            vec!["commit", "--no-pathspec-file-nul=value", "-m", "subject"],
            vec!["commit", "--pathspec-file-nul", "-m", "subject"],
            vec![
                "commit",
                "--pathspec-file-nul",
                "tracked.txt",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=pathspecs",
                "tracked.txt",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=pathspecs",
                "--",
                "tracked.txt",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=missing",
                "--no-pathspec-from-file",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=pathspecs",
                "--interactive",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=pathspecs",
                "--patch",
                "-m",
                "subject",
            ],
            vec![
                "commit",
                "--pathspec-from-file=pathspecs",
                "--all",
                "-m",
                "subject",
            ],
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
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_file_messages_match_upstream_git_objects() {
    let root = unique_temp_dir("commit-file-messages");
    fs::create_dir_all(&root).expect("create temp root");
    {
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
                "gpg-sign-reset",
                vec!["commit", "--gpg-sign", "--no-gpg-sign", "-m", "subject"],
            ),
            (
                "gpg-sign-key-reset",
                vec!["commit", "--gpg-sign=key", "--no-gpg-sign", "-m", "subject"],
            ),
            (
                "gpg-sign-empty-key-reset",
                vec!["commit", "--gpg-sign=", "--no-gpg-sign", "-m", "subject"],
            ),
            (
                "gpg-sign-short-reset",
                vec!["commit", "-S", "--no-gpg-sign", "-m", "subject"],
            ),
            (
                "gpg-sign-short-key-reset",
                vec!["commit", "-Skey", "--no-gpg-sign", "-m", "subject"],
            ),
            (
                "gpg-sign-restored-reset",
                vec![
                    "commit",
                    "--no-gpg-sign",
                    "--gpg-sign",
                    "--no-gpg-sign",
                    "-m",
                    "subject",
                ],
            ),
            (
                "gpg-sign-key-restored-reset",
                vec![
                    "commit",
                    "--no-gpg-sign",
                    "--gpg-sign=key",
                    "--no-gpg-sign",
                    "-m",
                    "subject",
                ],
            ),
            (
                "pathspec-file-nul-reset",
                vec![
                    "commit",
                    "--pathspec-file-nul",
                    "--no-pathspec-file-nul",
                    "-m",
                    "subject",
                ],
            ),
            (
                "pathspec-from-file-reset",
                vec![
                    "commit",
                    "--pathspec-from-file=pathspecs",
                    "--no-pathspec-from-file",
                    "-m",
                    "subject",
                ],
            ),
            (
                "pathspec-from-file-nul-reset",
                vec![
                    "commit",
                    "--pathspec-file-nul",
                    "--pathspec-from-file=pathspecs",
                    "--no-pathspec-file-nul",
                    "--no-pathspec-from-file",
                    "-m",
                    "subject",
                ],
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
                "quiet-reset",
                vec!["commit", "--quiet", "--no-quiet", "-m", "subject"],
            ),
            (
                "verify-reset",
                vec!["commit", "--no-verify", "--verify", "-m", "subject"],
            ),
            (
                "dry-run-reset",
                vec!["commit", "--dry-run", "--no-dry-run", "-m", "subject"],
            ),
            (
                "no-dry-run",
                vec!["commit", "--no-dry-run", "-m", "subject"],
            ),
            (
                "short-reset",
                vec!["commit", "--short", "--no-short", "-m", "subject"],
            ),
            (
                "porcelain-reset",
                vec!["commit", "--porcelain", "--no-porcelain", "-m", "subject"],
            ),
            (
                "null-reset",
                vec!["commit", "--null", "--no-null", "-m", "subject"],
            ),
            (
                "long-reset",
                vec!["commit", "--long", "--no-long", "-m", "subject"],
            ),
            ("no-long", vec!["commit", "--no-long", "-m", "subject"]),
            (
                "ahead-behind",
                vec!["commit", "--ahead-behind", "-m", "subject"],
            ),
            (
                "no-ahead-behind",
                vec!["commit", "--no-ahead-behind", "-m", "subject"],
            ),
            (
                "interactive-reset",
                vec![
                    "commit",
                    "--interactive",
                    "--no-interactive",
                    "-m",
                    "subject",
                ],
            ),
            (
                "patch-reset",
                vec!["commit", "--patch", "--no-patch", "-m", "subject"],
            ),
            (
                "patch-short-reset",
                vec!["commit", "-p", "--no-patch", "-m", "subject"],
            ),
            (
                "post-rewrite",
                vec!["commit", "--post-rewrite", "-m", "subject"],
            ),
            (
                "no-post-rewrite",
                vec!["commit", "--no-post-rewrite", "-m", "subject"],
            ),
            (
                "post-rewrite-reset",
                vec![
                    "commit",
                    "--post-rewrite",
                    "--no-post-rewrite",
                    "-m",
                    "subject",
                ],
            ),
            ("status", vec!["commit", "--status", "-m", "subject"]),
            ("no-status", vec!["commit", "--no-status", "-m", "subject"]),
            (
                "status-reset",
                vec!["commit", "--status", "--no-status", "-m", "subject"],
            ),
            ("verbose", vec!["commit", "--verbose", "-m", "subject"]),
            (
                "no-verbose",
                vec!["commit", "--no-verbose", "-m", "subject"],
            ),
            (
                "verbose-reset",
                vec!["commit", "--verbose", "--no-verbose", "-m", "subject"],
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
                "untracked-files-reset",
                vec![
                    "commit",
                    "--untracked-files=all",
                    "--no-untracked-files",
                    "-m",
                    "subject",
                ],
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
            (
                "edit-reset",
                vec!["commit", "--edit", "--no-edit", "-m", "subject"],
            ),
            ("branch", vec!["commit", "--branch", "-m", "subject"]),
            ("no-branch", vec!["commit", "--no-branch", "-m", "subject"]),
            (
                "branch-reset",
                vec!["commit", "--branch", "--no-branch", "-m", "subject"],
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
                "template-short",
                vec!["commit", "-t", "message-lf.txt", "-m", "subject"],
            ),
            (
                "template-short-attached",
                vec!["commit", "-tmessage-lf.txt", "-m", "subject"],
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
            ("terminator", vec!["commit", "-m", "subject", "--"]),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);
            fs::write(expected_root.join("pathspecs"), b"tracked.txt\n")
                .expect("write expected pathspec file");
            fs::write(actual_root.join("pathspecs"), b"tracked.txt\n")
                .expect("write actual pathspec file");

            let expected =
                run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
            assert!(
                actual.status.success(),
                "sley {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            let quiet = args.iter().fold(false, |quiet, arg| match *arg {
                "-q" | "--quiet" => true,
                "--no-quiet" => false,
                _ => quiet,
            });
            if quiet {
                assert_eq!(actual.stdout, expected.stdout, "quiet stdout differed");
                assert_eq!(actual.stderr, expected.stderr, "quiet stderr differed");
            }
            assert_eq!(
                cat_head(sley_testkit::oracle_git(), &actual_root),
                cat_head(sley_testkit::oracle_git(), &expected_root),
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
        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert_same_output(actual, expected, &args);

        let expected_root = root.join("trailer-only-expected");
        let actual_root = root.join("trailer-only-actual");
        fs::create_dir_all(&expected_root).expect("create expected repo");
        fs::create_dir_all(&actual_root).expect("create actual repo");
        prepare_commit_repo(&expected_root);
        prepare_commit_repo(&actual_root);
        let args = ["commit", "--trailer", "Acked-by=Alice"];
        let expected =
            run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
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

            let expected = run_output_with_identity_and_editor(
                sley_testkit::oracle_git(),
                &expected_root,
                &args,
            );
            assert!(
                expected.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&expected.stderr)
            );
            let actual =
                run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
            assert!(
                actual.status.success(),
                "sley {args:?} failed: {}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                cat_head(sley_testkit::oracle_git(), &actual_root),
                cat_head(sley_testkit::oracle_git(), &expected_root),
                "committed object differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_status_preview_modes_match_upstream_git() {
    let root = unique_temp_dir("commit-status-preview");
    fs::create_dir_all(&root).expect("create temp root");
    {
        for (name, args) in [
            ("short-no-message", vec!["commit", "--short"]),
            ("short", vec!["commit", "--short", "-m", "subject"]),
            ("long", vec!["commit", "--long", "-m", "subject"]),
            ("porcelain", vec!["commit", "--porcelain", "-m", "subject"]),
            (
                "dry-run-long-default",
                vec!["commit", "--dry-run", "-m", "subject"],
            ),
            (
                "dry-run-short",
                vec!["commit", "--dry-run", "--short", "-m", "subject"],
            ),
            (
                "dry-run-long",
                vec!["commit", "--dry-run", "--long", "-m", "subject"],
            ),
            (
                "dry-run-porcelain",
                vec!["commit", "--dry-run", "--porcelain", "-m", "subject"],
            ),
            (
                "dry-run-null",
                vec!["commit", "--dry-run", "-z", "-m", "subject"],
            ),
            ("null-short", vec!["commit", "-z", "-m", "subject"]),
            ("null-long", vec!["commit", "--null", "-m", "subject"]),
            (
                "short-null",
                vec!["commit", "--short", "--null", "-m", "subject"],
            ),
            (
                "null-reset-short",
                vec!["commit", "--null", "--no-null", "--short", "-m", "subject"],
            ),
            (
                "long-reset-short",
                vec!["commit", "--long", "--no-long", "--short", "-m", "subject"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            prepare_commit_repo(&expected_root);
            prepare_commit_repo(&actual_root);

            let expected =
                run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
            assert_same_output(actual, expected, &args);

            for (program, repo) in [
                (sley_testkit::oracle_git(), expected_root.as_path()),
                (sley_testkit::sley_bin!(), actual_root.as_path()),
            ] {
                let head = run_output(program, repo, &["rev-parse", "--verify", "HEAD"]);
                assert_eq!(
                    head.status.code(),
                    Some(128),
                    "{program} unexpectedly created HEAD for {args:?}"
                );
            }
        }

        for (name, args) in [
            (
                "dry-run-long-default-clean",
                vec!["commit", "--dry-run", "-m", "subject"],
            ),
            (
                "dry-run-long-clean",
                vec!["commit", "--dry-run", "--long", "-m", "subject"],
            ),
        ] {
            let expected_root = root.join(format!("{name}-expected"));
            let actual_root = root.join(format!("{name}-actual"));
            fs::create_dir_all(&expected_root).expect("create expected repo");
            fs::create_dir_all(&actual_root).expect("create actual repo");
            run_success(
                sley_testkit::oracle_git(),
                &expected_root,
                &["init", "-q", "-b", "main"],
            );
            run_success(
                sley_testkit::oracle_git(),
                &actual_root,
                &["init", "-q", "-b", "main"],
            );

            let expected =
                run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
            let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_all_stages_tracked_changes_like_upstream_git_objects() {
    let root = unique_temp_dir("commit-all-tracked");
    fs::create_dir_all(&root).expect("create temp root");
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
        create_initial_commit(sley_testkit::oracle_git(), &expected_root);
        create_initial_commit(sley_testkit::sley_bin!(), &actual_root);
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);
        if deleted {
            fs::remove_file(expected_root.join("tracked.txt")).expect("delete expected tracked");
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

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_reuse_message_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-reuse-message");
    fs::create_dir_all(&root).expect("create temp root");
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
        let expected_initial =
            run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &initial_args);
        assert!(
            expected_initial.status.success(),
            "git initial commit failed: {}",
            String::from_utf8_lossy(&expected_initial.stderr)
        );
        let actual_initial =
            run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &initial_args);
        assert!(
            actual_initial.status.success(),
            "sley initial commit failed: {}",
            String::from_utf8_lossy(&actual_initial.stderr)
        );
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);
        fs::write(expected_root.join("tracked.txt"), b"changed\n")
            .expect("modify expected tracked");
        fs::write(actual_root.join("tracked.txt"), b"changed\n").expect("modify actual tracked");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_reedit_message_matches_upstream_git_objects_when_editor_is_noop() {
    let root = unique_temp_dir("commit-reedit-message");
    fs::create_dir_all(&root).expect("create temp root");
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
        let expected_initial =
            run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &initial_args);
        assert!(
            expected_initial.status.success(),
            "git initial commit failed: {}",
            String::from_utf8_lossy(&expected_initial.stderr)
        );
        let actual_initial =
            run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &initial_args);
        assert!(
            actual_initial.status.success(),
            "sley initial commit failed: {}",
            String::from_utf8_lossy(&actual_initial.stderr)
        );
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);
        fs::write(expected_root.join("tracked.txt"), b"changed\n")
            .expect("modify expected tracked");
        fs::write(actual_root.join("tracked.txt"), b"changed\n").expect("modify actual tracked");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );

        let expected =
            run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_amend_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-amend");
    fs::create_dir_all(&root).expect("create temp root");
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
        create_initial_commit(sley_testkit::oracle_git(), &expected_root);
        create_initial_commit(sley_testkit::sley_bin!(), &actual_root);
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);

        fs::write(expected_root.join("tracked.txt"), b"old\n").expect("modify expected old");
        fs::write(actual_root.join("tracked.txt"), b"old\n").expect("modify actual old");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );
        let old_args = [
            "commit",
            "--author=Reuse User <reuse@example.invalid>",
            "--date=@123 +0000",
            "-m",
            "old subject",
            "-m",
            "old body",
        ];
        let expected_old =
            run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &old_args);
        assert!(
            expected_old.status.success(),
            "git old commit failed: {}",
            String::from_utf8_lossy(&expected_old.stderr)
        );
        let actual_old =
            run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &old_args);
        assert!(
            actual_old.status.success(),
            "sley old commit failed: {}",
            String::from_utf8_lossy(&actual_old.stderr)
        );

        fs::write(expected_root.join("tracked.txt"), b"amended\n").expect("modify expected amend");
        fs::write(actual_root.join("tracked.txt"), b"amended\n").expect("modify actual amend");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );

        let expected =
            run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_amend_with_pathspec_matches_upstream_git() {
    let root = unique_temp_dir("commit-amend-pathspec");
    let expected_root = root.join("expected");
    let actual_root = root.join("actual");
    for repo in [&expected_root, &actual_root] {
        fs::create_dir_all(repo).expect("create repository");
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["init", "-q", "-b", "main"],
        );
        fs::write(repo.join("selected"), b"base\n").expect("write selected base");
        fs::write(repo.join("other"), b"base\n").expect("write other base");
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["add", "selected", "other"],
        );
        let output = run_output_with_identity(
            sley_testkit::oracle_git(),
            repo,
            &["commit", "-q", "-m", "base"],
        );
        assert!(output.status.success());

        fs::write(repo.join("selected"), b"old selected\n").expect("write old selected");
        fs::write(repo.join("other"), b"old other\n").expect("write old other");
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["add", "selected", "other"],
        );
        let output = run_output_with_identity(
            sley_testkit::oracle_git(),
            repo,
            &["commit", "-q", "-m", "old"],
        );
        assert!(output.status.success());

        fs::write(repo.join("selected"), b"amended selected\n").expect("amend selected");
        fs::write(repo.join("other"), b"staged but excluded\n").expect("stage excluded path");
        run_success(sley_testkit::oracle_git(), repo, &["add", "other"]);
    }

    let args = ["commit", "--amend", "-q", "-m", "amended", "selected"];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
    assert!(
        expected.status.success(),
        "git amend failed: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
    assert!(
        actual.status.success(),
        "sley amend failed: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        cat_head(sley_testkit::oracle_git(), &actual_root),
        cat_head(sley_testkit::oracle_git(), &expected_root),
        "amended commit object differs"
    );
    let expected_index = run_output(
        sley_testkit::oracle_git(),
        &expected_root,
        &["diff", "--cached", "--raw"],
    );
    let actual_index = run_output(
        sley_testkit::oracle_git(),
        &actual_root,
        &["diff", "--cached", "--raw"],
    );
    assert_eq!(actual_index.stdout, expected_index.stdout);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_fixup_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-fixup");
    fs::create_dir_all(&root).expect("create temp root");
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
        let expected_initial =
            run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &initial_args);
        assert!(
            expected_initial.status.success(),
            "git initial commit failed: {}",
            String::from_utf8_lossy(&expected_initial.stderr)
        );
        let actual_initial =
            run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &initial_args);
        assert!(
            actual_initial.status.success(),
            "sley initial commit failed: {}",
            String::from_utf8_lossy(&actual_initial.stderr)
        );
        fs::write(expected_root.join("tracked.txt"), b"changed\n")
            .expect("modify expected tracked");
        fs::write(actual_root.join("tracked.txt"), b"changed\n").expect("modify actual tracked");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );

        let expected =
            run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_squash_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-squash");
    fs::create_dir_all(&root).expect("create temp root");
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
        let expected_initial =
            run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &initial_args);
        assert!(
            expected_initial.status.success(),
            "git initial commit failed: {}",
            String::from_utf8_lossy(&expected_initial.stderr)
        );
        let actual_initial =
            run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &initial_args);
        assert!(
            actual_initial.status.success(),
            "sley initial commit failed: {}",
            String::from_utf8_lossy(&actual_initial.stderr)
        );
        fs::write(expected_root.join("tracked.txt"), b"changed\n")
            .expect("modify expected tracked");
        fs::write(actual_root.join("tracked.txt"), b"changed\n").expect("modify actual tracked");
        run_success(
            sley_testkit::oracle_git(),
            &expected_root,
            &["add", "tracked.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual_root,
            &["add", "tracked.txt"],
        );

        let expected =
            run_output_with_identity_and_editor(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            run_output_with_identity_and_editor(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_allow_empty_matches_upstream_git_objects() {
    let root = unique_temp_dir("commit-allow-empty");
    fs::create_dir_all(&root).expect("create temp root");
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
        create_initial_commit(sley_testkit::oracle_git(), &expected_root);
        create_initial_commit(sley_testkit::sley_bin!(), &actual_root);
        remove_message_fixtures(&expected_root);
        remove_message_fixtures(&actual_root);

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_author_and_date_options_match_upstream_git_objects() {
    let root = unique_temp_dir("commit-author-date");
    fs::create_dir_all(&root).expect("create temp root");
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

        let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
        assert!(
            expected.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            cat_head(sley_testkit::oracle_git(), &actual_root),
            cat_head(sley_testkit::oracle_git(), &expected_root),
            "committed object differed for {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_tree_argument_errors_match_upstream_git() {
    let root = unique_temp_dir("commit-tree-argument-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        for args in [
            vec!["commit-tree", empty_tree, "-m"],
            vec!["commit-tree", empty_tree, "-m", "one", "-m"],
            vec!["commit-tree", empty_tree, "-p"],
            vec!["commit-tree", "-m", "message"],
            vec!["commit-tree", empty_tree, empty_tree, "-m", "message"],
        ] {
            let expected = run_output(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_tree_file_messages_match_upstream_git() {
    let root = unique_temp_dir("commit-tree-file-messages");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
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
            let expected = run_output_with_identity(sley_testkit::oracle_git(), &root, &args);
            let actual = run_output_with_identity(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
        let parent = run_output_with_identity(
            sley_testkit::oracle_git(),
            &root,
            &["commit-tree", empty_tree, "-mparent"],
        );
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
        let expected = run_output_with_identity(sley_testkit::oracle_git(), &root, &args);
        let actual = run_output_with_identity(sley_testkit::sley_bin!(), &root, &args);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_tree_duplicate_parent_matches_upstream_git() {
    let root = unique_temp_dir("commit-tree-duplicate-parent");
    let expected_root = root.join("expected");
    let actual_root = root.join("actual");
    fs::create_dir_all(&expected_root).expect("create expected repo");
    fs::create_dir_all(&actual_root).expect("create actual repo");

    for repo in [&expected_root, &actual_root] {
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["init", "-q", "-b", "main"],
        );
    }
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let parent = run_output_with_identity(
        sley_testkit::oracle_git(),
        &expected_root,
        &["commit-tree", empty_tree, "-mparent"],
    );
    assert!(parent.status.success());
    let parent = String::from_utf8(parent.stdout)
        .expect("parent oid utf8")
        .trim()
        .to_owned();

    let args = [
        "commit-tree",
        empty_tree,
        "-p",
        parent.as_str(),
        "-p",
        parent.as_str(),
        "-mchild",
    ];
    let expected = run_output_with_identity(sley_testkit::oracle_git(), &expected_root, &args);
    let actual = run_output_with_identity(sley_testkit::sley_bin!(), &actual_root, &args);
    assert_same_output(actual, expected, &args);

    let _ = fs::remove_dir_all(&root);
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Run a commit with the global config (`$HOME/.gitconfig`) as the *only* source
/// of identity: no `GIT_AUTHOR_*`/`GIT_COMMITTER_*` name/email env vars, no
/// repo-level `user.*`, and `GIT_CONFIG_NOSYSTEM=1` so the machine's
/// `/etc/gitconfig` cannot interfere. Author/committer dates are pinned so the
/// resulting commit is byte-for-byte comparable with upstream git. `extra_args`
/// are inserted before the commit subcommand (used here to exercise `-c`).
fn commit_with_global_identity(
    program: &str,
    repo: &Path,
    home: &Path,
    extra_args: &[&str],
) -> Output {
    let mut args: Vec<&str> = extra_args.to_vec();
    args.extend_from_slice(&["commit", "-m", "from-global"]);
    Command::new(program)
        .current_dir(repo)
        .args(&args)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "@1000 +0000")
        .env("GIT_COMMITTER_DATE", "@1000 +0000")
        // Ensure nothing from the test runner's environment supplies identity or
        // redirects the global/system config lookups.
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

/// sley must resolve `user.name`/`user.email` from the global `~/.gitconfig`
/// when neither identity env vars nor repo-level config provide them — matching
/// upstream git — and repo config and `-c` overrides must still win over global.
#[test]
fn commit_identity_falls_back_to_global_gitconfig_like_upstream_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("commit-global-identity");
    let home = root.join("home");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let result = std::panic::catch_unwind(|| {
        fs::create_dir_all(&home).expect("create temp home");
        fs::write(
            home.join(".gitconfig"),
            b"[user]\n\tname = Global Person\n\temail = global@example.invalid\n",
        )
        .expect("write global gitconfig");

        for repo in [&upstream, &rust] {
            fs::create_dir_all(repo).expect("create repo dir");
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &["init", "-q", "-b", "main"],
            );
            fs::write(repo.join("tracked.txt"), b"tracked\n").expect("write tracked file");
            run_success(sley_testkit::oracle_git(), repo, &["add", "tracked.txt"]);
        }

        // (1) Pure global fallback: identity comes only from ~/.gitconfig.
        // `git commit` and sley `commit` print different success summaries, so we
        // compare the resulting commit objects (via cat-file) rather than stdout.
        let expected =
            commit_with_global_identity(sley_testkit::oracle_git(), &upstream, &home, &[]);
        assert!(
            expected.status.success(),
            "git commit with global-only identity failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = commit_with_global_identity(sley_testkit::sley_bin!(), &rust, &home, &[]);
        assert!(
            actual.status.success(),
            "sley commit with global-only identity failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        let upstream_head = cat_head(sley_testkit::oracle_git(), &upstream);
        let rust_head = cat_head(sley_testkit::oracle_git(), &rust);
        assert_eq!(
            rust_head, upstream_head,
            "sley HEAD differs from git when identity comes from global config"
        );
        let head_text = String::from_utf8_lossy(&rust_head);
        assert!(
            head_text.contains("Global Person <global@example.invalid>"),
            "expected global identity in commit, got:\n{head_text}"
        );

        // (2) Repo-level user.* must override the global config (git parity).
        for repo in [&upstream, &rust] {
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &["config", "user.name", "Repo Person"],
            );
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &["config", "user.email", "repo@example.invalid"],
            );
            fs::write(repo.join("tracked.txt"), b"tracked 2\n").expect("update tracked file");
            run_success(sley_testkit::oracle_git(), repo, &["add", "tracked.txt"]);
        }
        let expected =
            commit_with_global_identity(sley_testkit::oracle_git(), &upstream, &home, &[]);
        assert!(
            expected.status.success(),
            "git commit with repo identity failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = commit_with_global_identity(sley_testkit::sley_bin!(), &rust, &home, &[]);
        assert!(
            actual.status.success(),
            "sley commit with repo identity failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        let rust_head = cat_head(sley_testkit::oracle_git(), &rust);
        let head_text = String::from_utf8_lossy(&rust_head);
        assert!(
            head_text.contains("Repo Person <repo@example.invalid>"),
            "expected repo identity to override global, got:\n{head_text}"
        );
        assert_eq!(rust_head, cat_head(sley_testkit::oracle_git(), &upstream));

        // (3) `-c user.*` must override both repo and global config (git parity).
        for repo in [&upstream, &rust] {
            fs::write(repo.join("tracked.txt"), b"tracked 3\n").expect("update tracked file");
            run_success(sley_testkit::oracle_git(), repo, &["add", "tracked.txt"]);
        }
        let overrides = [
            "-c",
            "user.name=Cli Person",
            "-c",
            "user.email=cli@example.invalid",
        ];
        let expected =
            commit_with_global_identity(sley_testkit::oracle_git(), &upstream, &home, &overrides);
        assert!(
            expected.status.success(),
            "git commit with -c identity failed: {}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual =
            commit_with_global_identity(sley_testkit::sley_bin!(), &rust, &home, &overrides);
        assert!(
            actual.status.success(),
            "sley commit with -c identity failed: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        let rust_head = cat_head(sley_testkit::oracle_git(), &rust);
        let head_text = String::from_utf8_lossy(&rust_head);
        assert!(
            head_text.contains("Cli Person <cli@example.invalid>"),
            "expected -c identity to override repo and global, got:\n{head_text}"
        );
        assert_eq!(rust_head, cat_head(sley_testkit::oracle_git(), &upstream));
    });
    let _ = fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
