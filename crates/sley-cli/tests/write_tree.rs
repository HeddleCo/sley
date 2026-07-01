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

fn indexed_oid(root: &Path) -> String {
    let output = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        root,
        &["ls-files", "--stage"],
    ))
    .expect("ls-files output is utf8");
    output
        .split_whitespace()
        .nth(1)
        .expect("stage output has oid")
        .to_string()
}

fn remove_loose_object(root: &Path, oid: &str) {
    let (prefix, suffix) = oid.split_at(2);
    fs::remove_file(root.join(".git").join("objects").join(prefix).join(suffix))
        .expect("remove loose object");
}

fn prepare_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo dir");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join("a"), b"data").expect("write fixture");
    run_success(sley_testkit::oracle_git(), root, &["add", "a"]);
    let oid = indexed_oid(root);
    remove_loose_object(root, &oid);
}

fn prepare_prefix_repo(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("create src dir");
    fs::create_dir_all(root.join("other")).expect("create other dir");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join("src").join("a"), b"a").expect("write src fixture");
    fs::write(root.join("other").join("b"), b"b").expect("write other fixture");
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["add", "src/a", "other/b"],
    );
}

#[test]
fn write_tree_missing_ok_matches_upstream_git() {
    let root = unique_temp_dir("write-tree-missing-ok");
    for (case, args) in [
        ("default", vec!["write-tree"]),
        ("missing-ok", vec!["write-tree", "--missing-ok"]),
        (
            "missing-ok-no-missing-ok",
            vec!["write-tree", "--missing-ok", "--no-missing-ok"],
        ),
        (
            "no-missing-ok-missing-ok",
            vec!["write-tree", "--no-missing-ok", "--missing-ok"],
        ),
    ] {
        let expected = root.join(format!("{case}-expected"));
        let actual = root.join(format!("{case}-actual"));
        prepare_repo(&expected);
        prepare_repo(&actual);
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(sley_testkit::sley_bin!(), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn write_tree_prefix_matches_upstream_git() {
    let root = unique_temp_dir("write-tree-prefix");
    let expected = root.join("expected");
    let actual = root.join("actual");
    {
        prepare_prefix_repo(&expected);
        prepare_prefix_repo(&actual);

        for args in [
            vec!["write-tree", "--prefix=src/"],
            vec!["write-tree", "--prefix=src"],
            vec!["write-tree", "--prefix=src/", "--no-prefix"],
            vec!["write-tree", "--no-prefix", "--prefix=src/"],
            vec!["write-tree", "--prefix", "src/", "--no-prefix"],
            vec!["write-tree", "--prefix=missing/"],
        ] {
            let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
            let actual_output = run(sley_testkit::sley_bin!(), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
