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

fn loose_object_path(git_dir: &Path, oid: &str) -> PathBuf {
    git_dir.join("objects").join(&oid[..2]).join(&oid[2..])
}

fn create_single_commit_repo(root: &Path) -> String {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join("payload.txt"), b"payload\n").expect("write payload");
    run_success(sley_testkit::oracle_git(), root, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
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
    String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        root,
        &["rev-parse", "HEAD^{tree}"],
    ))
    .expect("tree oid is utf8")
    .trim()
    .to_string()
}

#[test]
fn fsck_clean_repository_no_progress_matches_upstream_git() {
    let root = unique_temp_dir("fsck-clean");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_single_commit_repo(&root);
        let args = ["fsck", "--no-progress"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn fsck_missing_tree_reports_broken_link_like_upstream_git() {
    let root = unique_temp_dir("fsck-missing-tree");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        let tree = create_single_commit_repo(&root);
        fs::remove_file(loose_object_path(&root.join(".git"), &tree)).expect("remove tree object");
        let args = ["fsck", "--no-progress"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        let stdout = String::from_utf8(actual.stdout).expect("stdout is utf8");
        assert!(
            stdout.contains("broken link from  commit"),
            "expected broken-link report, got {stdout}"
        );
        assert!(
            stdout.contains(&format!("missing tree {tree}")),
            "expected missing tree report, got {stdout}"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn fsck_dangling_blob_matches_upstream_git() {
    let root = unique_temp_dir("fsck-dangling-blob");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_single_commit_repo(&root);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["hash-object", "-w", "--stdin"],
        );
        let args = ["fsck", "--no-progress"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);

        let args = ["fsck", "--no-progress", "--no-dangling"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn fsck_unreachable_commit_matches_upstream_git() {
    let root = unique_temp_dir("fsck-unreachable-commit");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_single_commit_repo(&root);
        let tree = String::from_utf8(run_success(
            sley_testkit::oracle_git(),
            &root,
            &["rev-parse", "HEAD^{tree}"],
        ))
        .expect("tree oid is utf8");
        let tree = tree.trim();
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit-tree",
                tree,
                "-m",
                "orphan",
            ],
        );

        let args = ["fsck", "--no-progress", "--unreachable", "--no-dangling"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
    };
    let _ = fs::remove_dir_all(&root);
}
