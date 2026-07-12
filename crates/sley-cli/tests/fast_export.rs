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
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", "@1112912053 -0700")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_DATE", "@1112912053 -0700")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn success(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn fixture(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    fs::create_dir_all(&root).expect("create fixture");
    success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
    fs::write(root.join("file"), b"blob contents\n").expect("write fixture blob");
    success(sley_testkit::oracle_git(), &root, &["add", "file"]);
    success(
        sley_testkit::oracle_git(),
        &root,
        &["commit", "-q", "-m", "initial"],
    );
    root
}

#[test]
fn exports_annotated_blob_tag_byte_identically() {
    let root = fixture("fast-export-blob-tag");
    let blob = success(
        sley_testkit::oracle_git(),
        &root,
        &["rev-parse", "HEAD:file"],
    );
    let blob = String::from_utf8(blob.stdout).expect("ascii oid");
    success(
        sley_testkit::oracle_git(),
        &root,
        &["tag", "-a", "blobtag", "-m", "Tag of a blob", blob.trim()],
    );

    let expected = success(
        sley_testkit::oracle_git(),
        &root,
        &["fast-export", "blobtag"],
    );
    let actual = success(
        sley_testkit::sley_bin!(),
        &root,
        &["fast-export", "blobtag"],
    );
    assert_eq!(actual.stdout, expected.stdout);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn exports_nested_tag_chain_with_marks_byte_identically() {
    let root = fixture("fast-export-nested-tag");
    success(
        sley_testkit::oracle_git(),
        &root,
        &["tag", "-a", "inner", "-m", "inner"],
    );
    success(
        sley_testkit::oracle_git(),
        &root,
        &["tag", "-a", "outer", "-m", "outer", "inner"],
    );

    let args = ["fast-export", "--mark-tags", "outer"];
    let expected = success(sley_testkit::oracle_git(), &root, &args);
    let actual = success(sley_testkit::sley_bin!(), &root, &args);
    assert_eq!(actual.stdout, expected.stdout);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn omits_tree_tag_without_aborting() {
    let root = fixture("fast-export-tree-tag");
    let tree = success(
        sley_testkit::oracle_git(),
        &root,
        &["rev-parse", "HEAD^{tree}"],
    );
    let tree = String::from_utf8(tree.stdout).expect("ascii oid");
    success(
        sley_testkit::oracle_git(),
        &root,
        &["tag", "-a", "tree-tag", "-m", "tree", tree.trim()],
    );

    let actual = success(
        sley_testkit::sley_bin!(),
        &root,
        &["fast-export", "tree-tag"],
    );
    assert!(actual.stdout.is_empty());
    assert!(String::from_utf8_lossy(&actual.stderr).contains("tags of trees"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn quotes_control_and_separator_bytes_like_git() {
    let root = fixture("fast-export-path-quoting");
    let paths = [
        "path with\nnewline",
        "path with \"quote\"",
        "path with \\backslash",
        "path with space",
    ];
    for path in paths {
        fs::write(root.join(path), b"content\n").expect("write unusual path");
    }
    success(sley_testkit::oracle_git(), &root, &["add", "--all"]);
    success(
        sley_testkit::oracle_git(),
        &root,
        &["commit", "-q", "-m", "unusual paths"],
    );

    let args = ["fast-export", "HEAD"];
    let expected = success(sley_testkit::oracle_git(), &root, &args);
    let actual = success(sley_testkit::sley_bin!(), &root, &args);
    assert_eq!(actual.stdout, expected.stdout);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn first_parent_merge_walk_matches_git_for_both_object_formats() {
    for format in ["sha1", "sha256"] {
        let root = unique_temp_dir(&format!("fast-export-first-parent-{format}"));
        fs::create_dir_all(&root).expect("create fixture");
        success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        );

        fs::write(root.join("A"), b"A\n").expect("write A");
        success(sley_testkit::oracle_git(), &root, &["add", "A"]);
        success(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-q", "-m", "A"],
        );
        success(
            sley_testkit::oracle_git(),
            &root,
            &["checkout", "-q", "-b", "topic1"],
        );
        fs::write(root.join("B"), b"B\n").expect("write B");
        success(sley_testkit::oracle_git(), &root, &["add", "B"]);
        success(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-q", "-m", "B"],
        );
        success(
            sley_testkit::oracle_git(),
            &root,
            &["checkout", "-q", "main"],
        );
        success(
            sley_testkit::oracle_git(),
            &root,
            &["merge", "-q", "--no-ff", "-m", "merge topic1", "topic1"],
        );

        success(
            sley_testkit::oracle_git(),
            &root,
            &["checkout", "-q", "-b", "topic2"],
        );
        fs::write(root.join("C"), b"C\n").expect("write C");
        success(sley_testkit::oracle_git(), &root, &["add", "C"]);
        success(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-q", "-m", "C"],
        );
        success(
            sley_testkit::oracle_git(),
            &root,
            &["checkout", "-q", "main"],
        );
        success(
            sley_testkit::oracle_git(),
            &root,
            &["merge", "-q", "--no-ff", "-m", "merge topic2", "topic2"],
        );
        fs::write(root.join("D"), b"D\n").expect("write D");
        success(sley_testkit::oracle_git(), &root, &["add", "D"]);
        success(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-q", "-m", "D"],
        );

        let args = ["fast-export", "main", "--", "--first-parent"];
        let reverse_args = ["fast-export", "main", "--", "--first-parent", "--reverse"];
        let expected = success(sley_testkit::oracle_git(), &root, &args);
        let actual = success(sley_testkit::sley_bin!(), &root, &args);
        let expected_reverse = success(sley_testkit::oracle_git(), &root, &reverse_args);
        let actual_reverse = success(sley_testkit::sley_bin!(), &root, &reverse_args);
        assert_eq!(actual.stdout, expected.stdout, "{format} forward stream");
        assert_eq!(
            actual_reverse.stdout, expected_reverse.stdout,
            "{format} reverse stream"
        );
        assert_eq!(
            actual.stdout, actual_reverse.stdout,
            "{format} stable order"
        );
        let _ = fs::remove_dir_all(root);
    }
}
