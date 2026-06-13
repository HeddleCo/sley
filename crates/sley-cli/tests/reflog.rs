use std::collections::BTreeMap;
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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_committer(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .env("GIT_COMMITTER_NAME", "Write User")
        .env("GIT_COMMITTER_EMAIL", "write@example.invalid")
        .env("GIT_COMMITTER_DATE", "@9 +0000")
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

fn run_success_with_identity_at(cwd: &Path, args: &[&str], date: &str) -> Vec<u8> {
    let output = Command::new(sley_testkit::oracle_git())
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
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

fn run_success_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_success_with_identity_at(cwd, args, "1970-01-01T00:00:00 +0000")
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

fn prepare_reflog_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    run_success_with_identity(root, &["commit", "--allow-empty", "-qm", "one"]);
    run_success_with_identity(root, &["commit", "--allow-empty", "-qm", "two"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["branch", "topic", "HEAD~1"],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["checkout", "-q", "topic"],
    );
    run_success_with_identity(root, &["commit", "--allow-empty", "-qm", "topic"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["checkout", "-q", "main"],
    );
}

fn prepare_drop_reflog_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    run_success_with_identity_at(
        root,
        &["commit", "--allow-empty", "-qm", "one"],
        "1970-01-01T00:00:01 +0000",
    );
    run_success_with_identity_at(
        root,
        &["commit", "--allow-empty", "-qm", "two"],
        "1970-01-01T00:00:02 +0000",
    );
    run_success_with_identity_at(
        root,
        &["branch", "topic", "HEAD~1"],
        "1970-01-01T00:00:03 +0000",
    );
}

fn prepare_linear_reflog_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    for (index, message) in ["one", "two", "three"].iter().enumerate() {
        let date = format!("1970-01-01T00:00:0{} +0000", index + 1);
        let output = Command::new(sley_testkit::oracle_git())
            .current_dir(root)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args([
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
            ])
            .args(["commit", "--allow-empty", "-qm", message])
            .output()
            .unwrap_or_else(|err| panic!("failed to run git commit {message}: {err}"));
        assert!(
            output.status.success(),
            "git commit {message} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn prepare_reflog_repo_with_unreachable_entry(root: &Path) {
    prepare_linear_reflog_repo(root);
    let output = Command::new(sley_testkit::oracle_git())
        .current_dir(root)
        .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:04 +0000")
        .env("GIT_REFLOG_ACTION", "reset")
        .args(["reset", "--hard", "-q", "HEAD~1"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run git reset: {err}"));
    assert!(
        output.status.success(),
        "git reset failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn reflog_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let logs_dir = root.join(".git/logs");
    let mut files = BTreeMap::new();
    collect_reflog_files(&logs_dir, &logs_dir, &mut files);
    files
}

fn collect_reflog_files(path: &Path, base: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries {
        let entry = entry.expect("read reflog entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("reflog file type");
        if file_type.is_dir() {
            collect_reflog_files(&path, base, files);
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(base)
                .expect("reflog under logs dir")
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            files.insert(relative, fs::read(path).expect("read reflog file"));
        }
    }
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create copy destination");
    for entry in fs::read_dir(src).expect("read copy source") {
        let entry = entry.expect("read copy entry");
        let source_path = entry.path();
        let dest_path = dst.join(entry.file_name());
        let file_type = entry.file_type().expect("copy entry type");
        if file_type.is_dir() {
            copy_dir_all(&source_path, &dest_path);
        } else if file_type.is_file() {
            fs::copy(&source_path, &dest_path).expect("copy file");
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path).expect("read symlink");
            std::os::unix::fs::symlink(target, &dest_path).expect("copy symlink");
        }
    }
}

#[test]
fn reflog_show_default_and_formats_match_upstream_git() {
    let root = unique_temp_dir("reflog-show");
    {
        prepare_reflog_repo(&root);

        for args in [
            vec!["reflog"],
            vec!["reflog", "show"],
            vec!["reflog", "show", "--oneline"],
            vec!["reflog", "show", "--format=%gs"],
            vec!["reflog", "show", "--pretty=format:%gs"],
            vec!["reflog", "-2"],
            vec!["reflog", "show", "--max-count=1"],
            vec!["reflog", "show", "HEAD"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_exists_status_matches_upstream_git() {
    let root = unique_temp_dir("reflog-exists");
    {
        prepare_reflog_repo(&root);

        for args in [
            vec!["reflog", "exists", "HEAD"],
            vec!["reflog", "exists", "refs/heads/main"],
            vec!["reflog", "exists", "main"],
            vec!["reflog", "exists", "missing"],
            vec!["reflog", "exists"],
            vec!["reflog", "exists", "HEAD", "extra"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_list_matches_upstream_git() {
    let root = unique_temp_dir("reflog-list");
    {
        prepare_reflog_repo(&root);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["tag", "--create-reflog", "v1"],
        );

        for args in [
            vec!["reflog", "list"],
            vec!["reflog", "list", "--"],
            vec!["reflog", "list", "--all"],
            vec!["reflog", "list", "HEAD"],
            vec!["reflog", "list", "--", "HEAD"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_delete_matches_upstream_git() {
    let root = unique_temp_dir("reflog-delete");
    for args in [
        vec!["reflog", "delete", "HEAD@{0}"],
        vec!["reflog", "delete", "-n", "HEAD@{0}"],
        vec!["reflog", "delete", "--verbose", "HEAD@{0}"],
        vec!["reflog", "delete", "HEAD@{99}"],
        vec!["reflog", "delete"],
        vec!["reflog", "delete", "--bogus"],
        vec!["reflog", "delete", "HEAD@{0}", "extra"],
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
        let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
        prepare_linear_reflog_repo(&upstream);
        prepare_linear_reflog_repo(&actual);

        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert_eq!(
            fs::read(actual.join(".git/logs/HEAD")).expect("actual HEAD reflog"),
            fs::read(upstream.join(".git/logs/HEAD")).expect("upstream HEAD reflog"),
            "HEAD reflog differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_delete_updateref_matches_upstream_git() {
    let root = unique_temp_dir("reflog-delete-updateref");
    for args in [
        vec!["reflog", "delete", "--updateref", "HEAD@{0}"],
        vec!["reflog", "delete", "--updateref", "main@{0}"],
        vec!["reflog", "delete", "--updateref", "refs/heads/main@{0}"],
        vec![
            "reflog",
            "delete",
            "--updateref",
            "--no-updateref",
            "main@{0}",
        ],
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
        let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
        prepare_linear_reflog_repo(&upstream);
        prepare_linear_reflog_repo(&actual);

        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert_eq!(
            fs::read(actual.join(".git/logs/HEAD")).expect("actual HEAD reflog"),
            fs::read(upstream.join(".git/logs/HEAD")).expect("upstream HEAD reflog"),
            "HEAD reflog differed after {args:?}"
        );
        assert_eq!(
            fs::read(actual.join(".git/logs/refs/heads/main")).expect("actual main reflog"),
            fs::read(upstream.join(".git/logs/refs/heads/main")).expect("upstream main reflog"),
            "main reflog differed after {args:?}"
        );
        assert_eq!(
            run(env!("CARGO_BIN_EXE_sley"), &actual, &["rev-parse", "main"]).stdout,
            run(
                sley_testkit::oracle_git(),
                &upstream,
                &["rev-parse", "main"]
            )
            .stdout,
            "main ref differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_drop_matches_upstream_git() {
    let root = unique_temp_dir("reflog-drop");
    {
        let upstream_template = root.join("upstream-template");
        let actual_template = root.join("actual-template");
        prepare_drop_reflog_repo(&upstream_template);
        prepare_drop_reflog_repo(&actual_template);

        for args in [
            vec!["reflog", "drop"],
            vec!["reflog", "drop", "refs/heads/main"],
            vec!["reflog", "drop", "HEAD"],
            vec!["reflog", "drop", "main", "topic"],
            vec!["reflog", "drop", "--all"],
            vec!["reflog", "drop", "--all", "--single-worktree"],
            vec!["reflog", "drop", "--all", "--no-all"],
            vec!["reflog", "drop", "--bogus"],
            vec!["reflog", "drop", "missing"],
            vec!["reflog", "drop", "--all", "missing"],
        ] {
            let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
            let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
            copy_dir_all(&upstream_template, &upstream);
            copy_dir_all(&actual_template, &actual);

            let expected = run(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected, &args);
            assert_eq!(
                reflog_files(&actual),
                reflog_files(&upstream),
                "reflog files differed after {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_write_matches_upstream_git() {
    let root = unique_temp_dir("reflog-write");
    {
        let upstream_template = root.join("upstream-template");
        let actual_template = root.join("actual-template");
        prepare_drop_reflog_repo(&upstream_template);
        prepare_drop_reflog_repo(&actual_template);
        let head = String::from_utf8(run_success(
            sley_testkit::oracle_git(),
            &upstream_template,
            &["rev-parse", "HEAD"],
        ))
        .expect("HEAD oid utf8")
        .trim()
        .to_string();
        let zero = "0000000000000000000000000000000000000000";
        let missing = "1111111111111111111111111111111111111111";

        for args in [
            vec![
                "reflog",
                "write",
                "refs/heads/main",
                &head,
                &head,
                "manual message",
            ],
            vec!["reflog", "write", "HEAD", &head, &head, "head message"],
            vec![
                "reflog",
                "write",
                "refs/heads/missing",
                &head,
                &head,
                "missing ref message",
            ],
            vec![
                "reflog",
                "write",
                "refs/heads/new/topic",
                zero,
                zero,
                "zero message",
            ],
            vec!["reflog", "write", "refs/heads/main", zero, zero, ""],
            vec!["reflog", "write"],
            vec!["reflog", "write", "refs/heads/main"],
            vec![
                "reflog",
                "write",
                "refs/heads/main",
                zero,
                zero,
                "msg",
                "extra",
            ],
            vec!["reflog", "write", "main", &head, &head, "msg"],
            vec!["reflog", "write", "refs/heads/bad..name", zero, zero, "msg"],
            vec!["reflog", "write", "refs/heads/main", "bad", &head, "msg"],
            vec!["reflog", "write", "refs/heads/main", &head, "bad", "msg"],
            vec!["reflog", "write", "refs/heads/main", missing, &head, "msg"],
            vec!["reflog", "write", "refs/heads/main", &head, missing, "msg"],
        ] {
            let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
            let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
            copy_dir_all(&upstream_template, &upstream);
            copy_dir_all(&actual_template, &actual);

            let expected = run_with_committer(sley_testkit::oracle_git(), &upstream, &args);
            let actual_output = run_with_committer(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected, &args);
            assert_eq!(
                reflog_files(&actual),
                reflog_files(&upstream),
                "reflog files differed after {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_expire_matches_upstream_git() {
    let root = unique_temp_dir("reflog-expire");
    for args in [
        vec![
            "reflog",
            "expire",
            "--expire=1970-01-01 00:00:02 +0000",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--verbose",
            "--expire=1970-01-01 00:00:02 +0000",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "-n",
            "--expire=1970-01-01 00:00:02 +0000",
            "refs/heads/main",
        ],
        vec!["reflog", "expire", "--expire=never", "refs/heads/main"],
        vec!["reflog", "expire", "--expire=all", "main"],
        vec![
            "reflog",
            "expire",
            "--rewrite",
            "--expire=1970-01-01 00:00:02 +0000",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--updateref",
            "--expire=1970-01-01 00:00:03 +0000",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--all",
            "--expire=1970-01-01 00:00:02 +0000",
        ],
        vec!["reflog", "expire"],
        vec!["reflog", "expire", "--bogus"],
        vec!["reflog", "expire", "--expire"],
        vec!["reflog", "expire", "--expire=bad", "refs/heads/main"],
        vec!["reflog", "expire", "refs/heads/missing"],
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
        let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
        prepare_linear_reflog_repo(&upstream);
        prepare_linear_reflog_repo(&actual);

        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert_eq!(
            fs::read(actual.join(".git/logs/HEAD")).expect("actual HEAD reflog"),
            fs::read(upstream.join(".git/logs/HEAD")).expect("upstream HEAD reflog"),
            "HEAD reflog differed after {args:?}"
        );
        assert_eq!(
            fs::read(actual.join(".git/logs/refs/heads/main")).expect("actual main reflog"),
            fs::read(upstream.join(".git/logs/refs/heads/main")).expect("upstream main reflog"),
            "main reflog differed after {args:?}"
        );
        assert_eq!(
            run(env!("CARGO_BIN_EXE_sley"), &actual, &["rev-parse", "main"]).stdout,
            run(
                sley_testkit::oracle_git(),
                &upstream,
                &["rev-parse", "main"]
            )
            .stdout,
            "main ref differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reflog_expire_unreachable_matches_upstream_git() {
    let root = unique_temp_dir("reflog-expire-unreachable");
    for args in [
        vec![
            "reflog",
            "expire",
            "--expire=never",
            "--expire-unreachable=1970-01-01 00:00:04 +0000",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--expire=never",
            "--expire-unreachable=all",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--expire=never",
            "--expire-unreachable=never",
            "refs/heads/main",
        ],
        vec![
            "reflog",
            "expire",
            "--verbose",
            "--expire=never",
            "--expire-unreachable=1970-01-01 00:00:04 +0000",
            "refs/heads/main",
        ],
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
        let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
        prepare_reflog_repo_with_unreachable_entry(&upstream);
        prepare_reflog_repo_with_unreachable_entry(&actual);

        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert_eq!(
            fs::read(actual.join(".git/logs/HEAD")).expect("actual HEAD reflog"),
            fs::read(upstream.join(".git/logs/HEAD")).expect("upstream HEAD reflog"),
            "HEAD reflog differed after {args:?}"
        );
        assert_eq!(
            fs::read(actual.join(".git/logs/refs/heads/main")).expect("actual main reflog"),
            fs::read(upstream.join(".git/logs/refs/heads/main")).expect("upstream main reflog"),
            "main reflog differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
