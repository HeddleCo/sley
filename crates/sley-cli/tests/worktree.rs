use std::fs;
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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_success_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("child stdin"),
        stdin,
    );
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_success_with_committer(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .current_dir(cwd)
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
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

fn run_success_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(sley_testkit::oracle_git())
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00 +0000")
        .args([
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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

fn assert_same_output_normalized(
    actual: Output,
    expected: Output,
    args: &[&str],
    actual_root: &Path,
    expected_root: &Path,
) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    let actual_stdout = normalize_output_paths(actual.stdout, actual_root, expected_root);
    let actual_stderr = normalize_output_paths(actual.stderr, actual_root, expected_root);
    assert_eq!(
        actual_stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    assert_eq!(
        actual_stderr, expected.stderr,
        "stderr differed for {args:?}"
    );
}

fn normalize_output_paths(
    mut output: Vec<u8>,
    actual_root: &Path,
    expected_root: &Path,
) -> Vec<u8> {
    let actual = actual_root.to_string_lossy();
    let expected = expected_root.to_string_lossy();
    let text = String::from_utf8_lossy(&output).replace(actual.as_ref(), expected.as_ref());
    output.clear();
    output.extend_from_slice(text.as_bytes());
    output
}

fn prepare_repo_with_linked_worktree(repo: &Path, linked: &Path) {
    fs::create_dir_all(repo).expect("create repo");
    run_success(sley_testkit::oracle_git(), repo, &["init", "-q", "-b", "main"]);
    run_success_with_identity(repo, &["commit", "--allow-empty", "-qm", "initial"]);
    run_success(sley_testkit::oracle_git(), repo, &["branch", "topic"]);
    let linked_path = linked.to_string_lossy().into_owned();
    run_success(
        sley_testkit::oracle_git(),
        repo,
        &["worktree", "add", "-q", &linked_path, "topic"],
    );
}

fn prepare_repo_with_stale_linked_worktree(repo: &Path, linked: &Path) {
    prepare_repo_with_linked_worktree(repo, linked);
    fs::remove_dir_all(linked).expect("remove linked worktree");
}

fn prepare_repo_for_worktree_add(repo: &Path) {
    fs::create_dir_all(repo).expect("create repo");
    run_success(sley_testkit::oracle_git(), repo, &["init", "-q", "-b", "main"]);
    prepare_worktree_add_contents(repo);
}

fn prepare_sha256_repo_for_worktree_add(repo: &Path) {
    fs::create_dir_all(repo).expect("create repo");
    run_success(sley_testkit::oracle_git(), repo, &["init", "-q", "--object-format=sha256", "-b", "main"]);
    prepare_worktree_add_contents(repo);
}

fn prepare_worktree_add_contents(repo: &Path) {
    fs::write(repo.join("file.txt"), b"base\n").expect("write tracked file");
    run_success(sley_testkit::oracle_git(), repo, &["add", "file.txt"]);
    run_success_with_identity(repo, &["commit", "-qm", "initial"]);
    run_success(sley_testkit::oracle_git(), repo, &["branch", "topic"]);
}

#[test]
fn worktree_list_matches_upstream_git() {
    let root = unique_temp_dir("worktree-list");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let repo = root.join("repo");
        let linked = root.join("linked");
        fs::create_dir_all(&repo).expect("create repo");
        run_success(sley_testkit::oracle_git(), &repo, &["init", "-q", "-b", "main"]);
        run_success_with_identity(&repo, &["commit", "--allow-empty", "-qm", "initial"]);
        run_success(sley_testkit::oracle_git(), &repo, &["branch", "topic"]);
        let linked_path = linked.to_string_lossy().into_owned();
        run_success(
            sley_testkit::oracle_git(),
            &repo,
            &["worktree", "add", "-q", &linked_path, "topic"],
        );

        for args in [
            vec!["worktree", "list"],
            vec!["worktree", "list", "--porcelain"],
            vec!["worktree", "list", "--porcelain", "-z"],
            vec!["worktree", "list", "--porcelain", "--no-porcelain"],
            vec!["worktree", "list", "-z"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_list_detached_head_matches_upstream_git() {
    let root = unique_temp_dir("worktree-list-detached");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        run_success(sley_testkit::oracle_git(), &repo, &["init", "-q", "-b", "main"]);
        run_success_with_identity(&repo, &["commit", "--allow-empty", "-qm", "initial"]);
        run_success(sley_testkit::oracle_git(), &repo, &["checkout", "-q", "--detach", "HEAD"]);

        for args in [
            vec!["worktree", "list"],
            vec!["worktree", "list", "--porcelain"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_add_branch_detach_default_and_lock_match_upstream_git() {
    let root = unique_temp_dir("worktree-add");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream_area = root.join("upstream-area");
        let actual_area = root.join("actual-area");
        let upstream = upstream_area.join("repo");
        let actual = actual_area.join("repo");
        prepare_repo_for_worktree_add(&upstream);
        prepare_repo_for_worktree_add(&actual);

        let upstream_topic = upstream_area.join("topic-wt");
        let actual_topic = actual_area.join("topic-wt");
        let upstream_topic_arg = upstream_topic.to_string_lossy().into_owned();
        let actual_topic_arg = actual_topic.to_string_lossy().into_owned();
        let expected = run(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "add", &upstream_topic_arg, "topic"],
        );
        let actual_output = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "add", &actual_topic_arg, "topic"],
        );
        assert_same_output_normalized(
            actual_output,
            expected,
            &["worktree", "add", "<path>", "topic"],
            &actual_area,
            &upstream_area,
        );
        assert_eq!(
            fs::read(actual_topic.join("file.txt")).expect("read actual topic file"),
            b"base\n"
        );

        let upstream_default = upstream_area.join("default");
        let actual_default = actual_area.join("default");
        let upstream_default_arg = upstream_default.to_string_lossy().into_owned();
        let actual_default_arg = actual_default.to_string_lossy().into_owned();
        let expected = run(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "add", "-q", &upstream_default_arg],
        );
        let actual_output = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "add", "-q", &actual_default_arg],
        );
        assert_same_output(
            actual_output,
            expected,
            &["worktree", "add", "-q", "<path>"],
        );

        let upstream_detached = upstream_area.join("detached");
        let actual_detached = actual_area.join("detached");
        let upstream_detached_arg = upstream_detached.to_string_lossy().into_owned();
        let actual_detached_arg = actual_detached.to_string_lossy().into_owned();
        let expected = run(
            sley_testkit::oracle_git(),
            &upstream,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &upstream_detached_arg,
                "HEAD",
            ],
        );
        let actual_output = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &actual_detached_arg,
                "HEAD",
            ],
        );
        assert_same_output(
            actual_output,
            expected,
            &["worktree", "add", "-q", "--detach", "<path>", "HEAD"],
        );

        let upstream_locked = upstream_area.join("locked");
        let actual_locked = actual_area.join("locked");
        let upstream_locked_arg = upstream_locked.to_string_lossy().into_owned();
        let actual_locked_arg = actual_locked.to_string_lossy().into_owned();
        let expected = run(
            sley_testkit::oracle_git(),
            &upstream,
            &[
                "worktree",
                "add",
                "-q",
                "-f",
                "--lock",
                "--reason",
                "pinned",
                &upstream_locked_arg,
                "topic",
            ],
        );
        let actual_output = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &[
                "worktree",
                "add",
                "-q",
                "-f",
                "--lock",
                "--reason",
                "pinned",
                &actual_locked_arg,
                "topic",
            ],
        );
        assert_same_output(
            actual_output,
            expected,
            &[
                "worktree", "add", "-q", "-f", "--lock", "--reason", "pinned", "<path>", "topic",
            ],
        );

        let expected_list = run(sley_testkit::oracle_git(), &upstream, &["worktree", "list", "--porcelain"]);
        let actual_list = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "list", "--porcelain"],
        );
        assert_same_output_normalized(
            actual_list,
            expected_list,
            &["worktree", "list", "--porcelain"],
            &actual_area,
            &upstream_area,
        );
        assert_eq!(
            run(
                env!("CARGO_BIN_EXE_sley"),
                &actual_topic,
                &["status", "--short"]
            )
            .stdout,
            run(sley_testkit::oracle_git(), &upstream_topic, &["status", "--short"]).stdout
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_add_sha256_branch_matches_upstream_git() {
    let root = unique_temp_dir("worktree-add-sha256");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream_area = root.join("upstream-area");
        let actual_area = root.join("actual-area");
        let upstream = upstream_area.join("repo");
        let actual = actual_area.join("repo");
        prepare_sha256_repo_for_worktree_add(&upstream);
        prepare_sha256_repo_for_worktree_add(&actual);

        let upstream_topic = upstream_area.join("topic-wt");
        let actual_topic = actual_area.join("topic-wt");
        let upstream_topic_arg = upstream_topic.to_string_lossy().into_owned();
        let actual_topic_arg = actual_topic.to_string_lossy().into_owned();
        let expected = run(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "add", "-q", &upstream_topic_arg, "topic"],
        );
        let actual_output = run(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "add", "-q", &actual_topic_arg, "topic"],
        );
        assert_same_output(
            actual_output,
            expected,
            &["worktree", "add", "-q", "<path>", "topic"],
        );
        for args in [
            vec!["rev-parse", "--show-object-format=storage"],
            vec!["status", "--short"],
            vec!["ls-files", "--stage"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream_topic, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_topic, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn sha256_linked_worktree_commands_use_common_git_dir() {
    let root = unique_temp_dir("worktree-sha256-common-dir");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream_area = root.join("upstream-area");
        let actual_area = root.join("actual-area");
        let upstream = upstream_area.join("repo");
        let actual = actual_area.join("repo");
        prepare_sha256_repo_for_worktree_add(&upstream);
        prepare_sha256_repo_for_worktree_add(&actual);

        let upstream_linked = upstream_area.join("topic-wt");
        let actual_linked = actual_area.join("topic-wt");
        let upstream_linked_arg = upstream_linked.to_string_lossy().into_owned();
        let actual_linked_arg = actual_linked.to_string_lossy().into_owned();
        run_success(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "add", "-q", &upstream_linked_arg, "topic"],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "add", "-q", &actual_linked_arg, "topic"],
        );

        for args in [
            vec!["rev-parse", "--show-object-format=storage"],
            vec!["cat-file", "-t", "HEAD"],
            vec!["status", "--short", "-uall"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream_linked, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_linked, &args);
            assert_same_output(actual, expected, &args);
        }

        let expected_oid = run_success_with_stdin(
            sley_testkit::oracle_git(),
            &upstream_linked,
            &["hash-object", "-w", "--stdin"],
            b"linked object\n",
        );
        let actual_oid = run_success_with_stdin(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &["hash-object", "-w", "--stdin"],
            b"linked object\n",
        );
        assert_eq!(actual_oid, expected_oid);
        let actual_oid = String::from_utf8(actual_oid)
            .expect("oid is utf8")
            .trim()
            .to_string();
        assert_eq!(actual_oid.len(), 64);
        assert!(
            actual
                .join(".git")
                .join("objects")
                .join(&actual_oid[..2])
                .join(&actual_oid[2..])
                .is_file(),
            "hash-object from linked worktree should write to common object directory"
        );

        fs::write(upstream_linked.join("linked.txt"), b"linked\n").expect("write upstream file");
        fs::write(actual_linked.join("linked.txt"), b"linked\n").expect("write actual file");
        run_success(sley_testkit::oracle_git(), &upstream_linked, &["add", "linked.txt"]);
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &["add", "linked.txt"],
        );
        for args in [
            vec!["diff", "--cached", "--name-status", "HEAD"],
            vec!["write-tree"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream_linked, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_linked, &args);
            assert_same_output(actual, expected, &args);
        }

        let upstream_head = run_success(sley_testkit::oracle_git(), &upstream_linked, &["rev-parse", "HEAD"]);
        let actual_head = run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &["rev-parse", "HEAD"],
        );
        assert_eq!(actual_head, upstream_head);
        let upstream_head = String::from_utf8(upstream_head)
            .expect("head oid is utf8")
            .trim()
            .to_string();
        let actual_head = String::from_utf8(actual_head)
            .expect("head oid is utf8")
            .trim()
            .to_string();
        run_success_with_committer(
            sley_testkit::oracle_git(),
            &upstream_linked,
            &[
                "reflog",
                "write",
                "HEAD",
                &upstream_head,
                &upstream_head,
                "linked-head",
            ],
        );
        run_success_with_committer(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &[
                "reflog",
                "write",
                "HEAD",
                &actual_head,
                &actual_head,
                "linked-head",
            ],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &["reflog", "exists", "HEAD"],
        );
        assert_eq!(
            run_success(
                env!("CARGO_BIN_EXE_sley"),
                &actual_linked,
                &["reflog", "--format=%gs", "-1", "HEAD"],
            ),
            b"linked-head\n"
        );

        run_success_with_committer(
            sley_testkit::oracle_git(),
            &upstream_linked,
            &[
                "update-ref",
                "-m",
                "linked-branch",
                "refs/heads/from-linked",
                &upstream_head,
            ],
        );
        run_success_with_committer(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &[
                "update-ref",
                "-m",
                "linked-branch",
                "refs/heads/from-linked",
                &actual_head,
            ],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_linked,
            &["reflog", "exists", "refs/heads/from-linked"],
        );
        assert_eq!(
            run_success(
                env!("CARGO_BIN_EXE_sley"),
                &actual_linked,
                &["reflog", "--format=%gs", "-1", "refs/heads/from-linked"],
            ),
            b"linked-branch\n"
        );
        assert!(
            actual
                .join(".git")
                .join("logs")
                .join("refs")
                .join("heads")
                .join("from-linked")
                .is_file(),
            "update-ref from linked worktree should create shared branch reflog in the common git dir"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_add_error_paths_match_upstream_git() {
    let root = unique_temp_dir("worktree-add-errors");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream_area = root.join("upstream-area");
        let actual_area = root.join("actual-area");
        let upstream = upstream_area.join("repo");
        let actual = actual_area.join("repo");
        prepare_repo_for_worktree_add(&upstream);
        prepare_repo_for_worktree_add(&actual);

        for args in [
            vec!["worktree", "add"],
            vec!["worktree", "add", "--bogus"],
            vec!["worktree", "add", "../linked", "main"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &actual_area,
                &upstream_area,
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_stale_list_and_prune_match_upstream_git() {
    let root = unique_temp_dir("worktree-stale");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let repo = root.join("repo");
        let linked = root.join("linked");
        prepare_repo_with_stale_linked_worktree(&repo, &linked);

        for args in [
            vec!["worktree", "list"],
            vec!["worktree", "list", "-v"],
            vec!["worktree", "list", "--porcelain"],
            vec!["worktree", "list", "--porcelain", "-z"],
            vec!["worktree", "list", "--no-expire"],
            vec!["worktree", "prune", "-n", "-v", "--expire", "now"],
            vec!["worktree", "prune", "-n", "-v", "--no-expire"],
            vec!["worktree", "prune", "--expire"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_prune_removes_stale_admin_dirs_like_upstream_git() {
    let root = unique_temp_dir("worktree-prune");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_stale_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_stale_linked_worktree(&actual, &actual_linked);

        let args = vec!["worktree", "prune", "-v", "--expire", "now"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert!(
            !actual.join(".git/worktrees/linked").exists(),
            "stale worktree admin dir should be removed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_lock_unlock_and_list_match_upstream_git() {
    let root = unique_temp_dir("worktree-lock");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);

        for args in [
            vec!["worktree", "lock", "../linked"],
            vec!["worktree", "list"],
            vec!["worktree", "list", "-v"],
            vec!["worktree", "list", "--porcelain"],
            vec!["worktree", "unlock", "../linked"],
            vec!["worktree", "lock", "--reason", "why now", "../linked"],
            vec!["worktree", "list"],
            vec!["worktree", "list", "-v"],
            vec!["worktree", "list", "--porcelain"],
            vec!["worktree", "lock", "../linked"],
            vec!["worktree", "unlock", "../linked"],
            vec!["worktree", "unlock", "../linked"],
            vec!["worktree", "lock", "--reason"],
            vec!["worktree", "lock"],
            vec!["worktree", "unlock"],
            vec!["worktree", "lock", "."],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_prune_skips_locked_stale_worktrees_like_upstream_git() {
    let root = unique_temp_dir("worktree-prune-locked");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);
        let lock_args = vec!["worktree", "lock", "--reason=keep it", "../linked"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &lock_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &lock_args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &lock_args,
            &root.join("actual"),
            &root.join("upstream"),
        );
        fs::remove_dir_all(&upstream_linked).expect("remove upstream linked worktree");
        fs::remove_dir_all(&actual_linked).expect("remove actual linked worktree");

        for args in [
            vec!["worktree", "list"],
            vec!["worktree", "list", "-v"],
            vec!["worktree", "prune", "-n", "-v", "--expire", "now"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_remove_clean_and_error_paths_match_upstream_git() {
    let root = unique_temp_dir("worktree-remove");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);

        let args = vec!["worktree", "remove", "../linked"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &args,
            &root.join("actual"),
            &root.join("upstream"),
        );
        assert!(!actual_linked.exists(), "linked worktree should be removed");
        assert!(
            !actual.join(".git/worktrees/linked").exists(),
            "linked worktree admin dir should be removed"
        );

        let upstream_errors = root.join("upstream-errors/repo");
        let upstream_errors_linked = root.join("upstream-errors/linked");
        let actual_errors = root.join("actual-errors/repo");
        let actual_errors_linked = root.join("actual-errors/linked");
        prepare_repo_with_linked_worktree(&upstream_errors, &upstream_errors_linked);
        prepare_repo_with_linked_worktree(&actual_errors, &actual_errors_linked);
        for args in [
            vec!["worktree", "remove"],
            vec!["worktree", "remove", "../linked", "extra"],
            vec!["worktree", "remove", "--bogus", "../linked"],
            vec!["worktree", "remove", "."],
            vec!["worktree", "remove", "../missing"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream_errors, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual_errors, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual-errors"),
                &root.join("upstream-errors"),
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_remove_force_dirty_and_locked_match_upstream_git() {
    let root = unique_temp_dir("worktree-remove-force");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);
        fs::write(upstream_linked.join("untracked"), b"x\n").expect("write upstream dirty file");
        fs::write(actual_linked.join("untracked"), b"x\n").expect("write actual dirty file");

        for args in [
            vec!["worktree", "remove", "../linked"],
            vec!["worktree", "remove", "-f", "../linked"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
        assert!(
            !actual_linked.exists(),
            "forced dirty worktree should be removed"
        );

        let upstream_locked = root.join("upstream/locked");
        let actual_locked = root.join("actual/locked");
        run_success(sley_testkit::oracle_git(), &upstream, &["branch", "locked"]);
        run_success(sley_testkit::oracle_git(), &actual, &["branch", "locked"]);
        run_success(
            sley_testkit::oracle_git(),
            &upstream,
            &[
                "worktree",
                "add",
                "-q",
                upstream_locked.to_string_lossy().as_ref(),
                "locked",
            ],
        );
        run_success(
            sley_testkit::oracle_git(),
            &actual,
            &[
                "worktree",
                "add",
                "-q",
                actual_locked.to_string_lossy().as_ref(),
                "locked",
            ],
        );
        run_success(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "lock", "--reason", "keep", "../locked"],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "lock", "--reason", "keep", "../locked"],
        );
        for args in [
            vec!["worktree", "remove", "../locked"],
            vec!["worktree", "remove", "-f", "../locked"],
            vec!["worktree", "remove", "-f", "-f", "../locked"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
        assert!(
            !actual_locked.exists(),
            "double-forced locked worktree should be removed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_move_clean_dirty_and_directory_destination_match_upstream_git() {
    let root = unique_temp_dir("worktree-move");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);

        let args = vec!["worktree", "move", "../linked", "../moved"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &args,
            &root.join("actual"),
            &root.join("upstream"),
        );
        assert!(!actual_linked.exists(), "source worktree should be moved");
        assert!(
            root.join("actual/moved").exists(),
            "destination should exist"
        );
        let list_args = vec!["worktree", "list"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &list_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &list_args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &list_args,
            &root.join("actual"),
            &root.join("upstream"),
        );

        let upstream_dirty = root.join("upstream-dirty/repo");
        let upstream_dirty_linked = root.join("upstream-dirty/linked");
        let actual_dirty = root.join("actual-dirty/repo");
        let actual_dirty_linked = root.join("actual-dirty/linked");
        prepare_repo_with_linked_worktree(&upstream_dirty, &upstream_dirty_linked);
        prepare_repo_with_linked_worktree(&actual_dirty, &actual_dirty_linked);
        fs::write(upstream_dirty_linked.join("untracked"), b"x\n").expect("write upstream dirty");
        fs::write(actual_dirty_linked.join("untracked"), b"x\n").expect("write actual dirty");
        let args = vec!["worktree", "move", "../linked", "../moved"];
        let expected = run(sley_testkit::oracle_git(), &upstream_dirty, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual_dirty, &args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &args,
            &root.join("actual-dirty"),
            &root.join("upstream-dirty"),
        );
        assert!(
            root.join("actual-dirty/moved/untracked").exists(),
            "dirty file should move with worktree"
        );

        let upstream_dir = root.join("upstream-dir/repo");
        let upstream_dir_linked = root.join("upstream-dir/linked");
        let actual_dir = root.join("actual-dir/repo");
        let actual_dir_linked = root.join("actual-dir/linked");
        prepare_repo_with_linked_worktree(&upstream_dir, &upstream_dir_linked);
        prepare_repo_with_linked_worktree(&actual_dir, &actual_dir_linked);
        fs::create_dir_all(root.join("upstream-dir/destination")).expect("create upstream dest");
        fs::create_dir_all(root.join("actual-dir/destination")).expect("create actual dest");
        let args = vec!["worktree", "move", "../linked", "../destination"];
        let expected = run(sley_testkit::oracle_git(), &upstream_dir, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual_dir, &args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &args,
            &root.join("actual-dir"),
            &root.join("upstream-dir"),
        );
        assert!(
            root.join("actual-dir/destination/linked").exists(),
            "directory destination should receive source basename"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_move_locked_and_error_paths_match_upstream_git() {
    let root = unique_temp_dir("worktree-move-errors");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);
        run_success(
            sley_testkit::oracle_git(),
            &upstream,
            &["worktree", "lock", "--reason", "keep", "../linked"],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual,
            &["worktree", "lock", "--reason", "keep", "../linked"],
        );
        for args in [
            vec!["worktree", "move", "../linked", "../moved"],
            vec!["worktree", "move", "-f", "../linked", "../moved"],
            vec!["worktree", "move", "-f", "-f", "../linked", "../moved"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
        assert!(
            root.join("actual/moved").exists(),
            "double force should move locked worktree"
        );

        let upstream_errors = root.join("upstream-errors/repo");
        let upstream_errors_linked = root.join("upstream-errors/linked");
        let actual_errors = root.join("actual-errors/repo");
        let actual_errors_linked = root.join("actual-errors/linked");
        prepare_repo_with_linked_worktree(&upstream_errors, &upstream_errors_linked);
        prepare_repo_with_linked_worktree(&actual_errors, &actual_errors_linked);
        fs::write(root.join("upstream-errors/filedest"), b"x\n").expect("write upstream dest file");
        fs::write(root.join("actual-errors/filedest"), b"x\n").expect("write actual dest file");
        for args in [
            vec!["worktree", "move"],
            vec!["worktree", "move", "../linked", "../other", "extra"],
            vec!["worktree", "move", "--bogus", "../linked", "../other"],
            vec!["worktree", "move", ".", "../main2"],
            vec!["worktree", "move", "../missing", "../x"],
            vec!["worktree", "move", "../linked", "../filedest"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream_errors, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual_errors, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual-errors"),
                &root.join("upstream-errors"),
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_repair_updates_moved_worktree_metadata_like_upstream_git() {
    let root = unique_temp_dir("worktree-repair");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let upstream_moved = root.join("upstream/moved");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        let actual_moved = root.join("actual/moved");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);
        fs::rename(&upstream_linked, &upstream_moved).expect("move upstream linked worktree");
        fs::rename(&actual_linked, &actual_moved).expect("move actual linked worktree");

        let args = vec!["worktree", "repair", "../moved"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &args,
            &root.join("actual"),
            &root.join("upstream"),
        );
        let list_args = vec!["worktree", "list"];
        let expected = run(sley_testkit::oracle_git(), &upstream, &list_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &list_args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &list_args,
            &root.join("actual"),
            &root.join("upstream"),
        );

        let upstream_noarg = root.join("upstream-noarg/repo");
        let upstream_noarg_linked = root.join("upstream-noarg/linked");
        let upstream_noarg_moved = root.join("upstream-noarg/moved");
        let actual_noarg = root.join("actual-noarg/repo");
        let actual_noarg_linked = root.join("actual-noarg/linked");
        let actual_noarg_moved = root.join("actual-noarg/moved");
        prepare_repo_with_linked_worktree(&upstream_noarg, &upstream_noarg_linked);
        prepare_repo_with_linked_worktree(&actual_noarg, &actual_noarg_linked);
        fs::rename(&upstream_noarg_linked, &upstream_noarg_moved)
            .expect("move upstream noarg linked worktree");
        fs::rename(&actual_noarg_linked, &actual_noarg_moved)
            .expect("move actual noarg linked worktree");
        let no_args = vec!["worktree", "repair"];
        let expected = run(sley_testkit::oracle_git(), &upstream_noarg_moved, &no_args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual_noarg_moved, &no_args);
        assert_same_output_normalized(
            actual_output,
            expected,
            &no_args,
            &root.join("actual-noarg"),
            &root.join("upstream-noarg"),
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worktree_repair_error_paths_match_upstream_git() {
    let root = unique_temp_dir("worktree-repair-errors");
    fs::create_dir_all(&root).expect("create temp root");
    {
        let upstream = root.join("upstream/repo");
        let upstream_linked = root.join("upstream/linked");
        let actual = root.join("actual/repo");
        let actual_linked = root.join("actual/linked");
        prepare_repo_with_linked_worktree(&upstream, &upstream_linked);
        prepare_repo_with_linked_worktree(&actual, &actual_linked);

        for args in [
            vec!["worktree", "repair", "../missing"],
            vec!["worktree", "repair", "--foo"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output_normalized(
                actual_output,
                expected,
                &args,
                &root.join("actual"),
                &root.join("upstream"),
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}
