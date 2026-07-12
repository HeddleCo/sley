use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

fn run_status(program: &str, cwd: &Path, args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    (
        output.status.code().expect("process terminated by signal"),
        output.stdout,
        output.stderr,
    )
}

fn sley(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::sley_bin!(), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn utf8_trimmed(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .expect("git output is utf8")
        .trim()
        .to_string()
}

#[test]
fn diff_name_only_matches_upstream_git() {
    let root = unique_temp_dir("diff-name-only");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        git(&root, &["add", "delete.txt", "modify.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::write(root.join("new.txt"), b"new\n").expect("write new fixture");
        git(&root, &["add", "new.txt"]);
        fs::write(root.join("untracked.txt"), b"ignored\n").expect("write untracked fixture");

        for args in [
            vec!["diff", "--name-status"],
            vec!["diff", "--name-only"],
            vec!["diff", "-R", "--name-status"],
            vec!["diff", "-R", "--name-only"],
            vec!["diff", "--name-status", "HEAD"],
            vec!["diff", "--name-only", "HEAD"],
            vec!["diff", "-R", "--name-status", "HEAD"],
            vec!["diff", "-R", "--name-only", "HEAD"],
            vec!["diff", "--raw", "--name-status", "HEAD"],
            vec!["diff", "--name-status", "--raw", "HEAD"],
            vec!["diff", "--raw", "--name-only", "HEAD"],
            vec!["diff", "--name-only", "--raw", "HEAD"],
            vec!["diff", "--stat", "--name-status", "HEAD"],
            vec!["diff", "--name-status", "--stat", "HEAD"],
            vec!["diff", "--stat", "--name-only", "HEAD"],
            vec!["diff", "--name-only", "--stat", "HEAD"],
            vec!["diff", "--numstat", "--name-only", "HEAD"],
            vec!["diff", "--name-only", "--numstat", "HEAD"],
            vec!["diff", "--name-only", "--no-patch", "--raw", "HEAD"],
            vec!["diff", "--name-status", "--no-patch", "--stat", "HEAD"],
            vec!["diff", "--cached", "--name-status"],
            vec!["diff", "--cached", "--name-only"],
            vec!["diff", "--cached", "-R", "--name-status"],
            vec!["diff", "--cached", "-R", "--name-only"],
            vec!["diff", "--cached", "--name-status", "HEAD"],
            vec!["diff", "--cached", "--name-only", "HEAD"],
            vec!["diff", "--cached", "-R", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-R", "--name-only", "HEAD"],
            vec!["diff", "--staged", "--name-status", "HEAD"],
            vec!["diff", "--staged", "--name-only", "HEAD"],
            vec!["diff", "--name-status", "--ext-diff", "HEAD"],
            vec!["diff", "--name-status", "--no-ext-diff", "HEAD"],
            vec!["diff", "--name-status", "--textconv", "HEAD"],
            vec!["diff", "--name-status", "--no-textconv", "HEAD"],
            vec!["diff", "--name-status", "--color", "HEAD"],
            vec!["diff", "--name-status", "--color=always", "HEAD"],
            vec!["diff", "--name-status", "--no-color", "HEAD"],
            vec!["diff", "--name-status", "--color=never", "HEAD"],
            vec!["diff", "--name-status", "--color=auto", "HEAD"],
            vec!["diff", "--name-status", "--color-moved", "HEAD"],
            vec!["diff", "--name-status", "--no-color-moved", "HEAD"],
            vec!["diff", "--name-status", "--color-moved=plain", "HEAD"],
            vec!["diff", "--name-status", "--color-moved=true", "HEAD"],
            vec!["diff", "--name-status", "--color-moved=false", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--color-moved-ws=ignore-all-space",
                "HEAD",
            ],
            vec![
                "diff",
                "--name-status",
                "--color-moved-ws",
                "ignore-all-space",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--no-color-moved-ws", "HEAD"],
            vec!["diff", "--name-status", "--ignore-submodules", "HEAD"],
            vec!["diff", "--name-status", "--ignore-submodules=dirty", "HEAD"],
            vec!["diff", "--name-status", "--minimal", "HEAD"],
            vec!["diff", "--name-status", "--patience", "HEAD"],
            vec!["diff", "--name-status", "--histogram", "HEAD"],
            vec!["diff", "--name-status", "--anchored", "before", "HEAD"],
            vec!["diff", "--name-status", "--anchored=before", "HEAD"],
            vec!["diff", "--name-status", "--diff-algorithm", "myers", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--diff-algorithm=histogram",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--inter-hunk-context", "3", "HEAD"],
            vec!["diff", "--name-status", "--inter-hunk-context=3", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--ws-error-highlight",
                "all",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--ws-error-highlight=all", "HEAD"],
            vec!["diff", "--name-status", "-b", "HEAD"],
            vec!["diff", "--name-status", "-w", "HEAD"],
            vec!["diff", "--name-status", "--ignore-space-at-eol", "HEAD"],
            vec!["diff", "--name-status", "--ignore-cr-at-eol", "HEAD"],
            vec!["diff", "--name-status", "--ignore-space-change", "HEAD"],
            vec!["diff", "--name-status", "--ignore-all-space", "HEAD"],
            vec!["diff", "--name-status", "--ignore-blank-lines", "HEAD"],
            vec!["diff", "--name-status", "--submodule", "HEAD"],
            vec!["diff", "--name-status", "--submodule=short", "HEAD"],
            vec!["diff", "--name-status", "--submodule=log", "HEAD"],
            vec!["diff", "--name-status", "--submodule=diff", "HEAD"],
            vec!["diff", "--name-status", "--word-diff", "HEAD"],
            vec!["diff", "--name-status", "--word-diff=plain", "HEAD"],
            vec!["diff", "--name-status", "--word-diff=color", "HEAD"],
            vec!["diff", "--name-status", "--word-diff=porcelain", "HEAD"],
            vec!["diff", "--name-status", "--word-diff=none", "HEAD"],
            vec!["diff", "--name-status", "--word-diff-regex", ".", "HEAD"],
            vec!["diff", "--name-status", "--word-diff-regex=.", "HEAD"],
            vec!["diff", "--name-status", "--color-words", "HEAD"],
            vec!["diff", "--name-status", "--color-words=.", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--output-indicator-new",
                "@",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--output-indicator-old=_", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--output-indicator-context",
                ".",
                "HEAD",
            ],
            vec!["diff", "--name-status", "-W", "HEAD"],
            vec!["diff", "--name-status", "--function-context", "HEAD"],
            vec!["diff", "--name-status", "--indent-heuristic", "HEAD"],
            vec!["diff", "--name-status", "--no-indent-heuristic", "HEAD"],
            vec!["diff", "--name-status", "--full-diff", "HEAD"],
            vec!["diff", "--name-status", "-D", "HEAD"],
            vec!["diff", "--name-status", "--irreversible-delete", "HEAD"],
            vec!["diff", "--name-status", "--ita-visible-in-index", "HEAD"],
            vec!["diff", "--name-status", "--ita-invisible-in-index", "HEAD"],
            vec!["diff", "--name-status", "-B", "HEAD"],
            vec!["diff", "--name-status", "-B50%", "HEAD"],
            vec!["diff", "--name-status", "-B20%/70%", "HEAD"],
            vec!["diff", "--name-status", "--break-rewrites", "HEAD"],
            vec!["diff", "--name-status", "--break-rewrites=50%", "HEAD"],
            vec!["diff", "--name-status", "--break-rewrites=20%/70%", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec!["diff", "--name-status", "-Bfoo", "HEAD"],
            vec!["diff", "--name-status", "-B1/2/3", "HEAD"],
            vec!["diff", "--name-status", "--break-rewrites=foo", "HEAD"],
            vec!["diff", "--name-status", "--break-rewrites=1/2/3", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_sha256_name_status_matches_upstream_git() {
    let root = unique_temp_dir("diff-sha256-name-status");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(
            &root,
            &["init", "-q", "--object-format=sha256", "-b", "main"],
        );
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        git(&root, &["add", "delete.txt", "modify.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::write(root.join("new.txt"), b"new\n").expect("write new fixture");
        git(&root, &["add", "new.txt"]);

        for args in [
            vec!["diff", "--name-status"],
            vec!["diff", "--name-only"],
            vec!["diff", "--name-status", "HEAD"],
            vec!["diff", "--name-only", "HEAD"],
            vec!["diff", "--cached", "--name-status"],
            vec!["diff", "--cached", "--name-only"],
            vec!["diff", "--cached", "--name-status", "HEAD"],
            vec!["diff", "--cached", "--name-only", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_quoted_paths_match_upstream_git() {
    for (case, path) in [
        ("space", "space name.txt"),
        ("quote", "quote\"name.txt"),
        ("tab", "tab\tname.txt"),
    ] {
        let root = unique_temp_dir(&format!("diff-quoted-paths-{case}"));
        fs::create_dir_all(&root).expect("create temp repo");
        {
            git(&root, &["init", "-q", "-b", "main"]);
            fs::write(root.join(path), b"before\n").expect("write fixture");
            git(&root, &["add", path]);
            git(
                &root,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "-m",
                    "base",
                    "-q",
                ],
            );

            fs::write(root.join(path), b"after\n").expect("modify fixture");
            for args in [
                vec!["diff", "--name-only"],
                vec!["diff", "--name-status"],
                vec!["diff", "--name-only", "-z"],
                vec!["diff", "--name-status", "-z"],
                vec!["diff", "-z", "--name-only"],
                vec!["diff", "-z", "--name-status"],
                vec!["diff", "--name-only", "HEAD"],
                vec!["diff", "--name-status", "HEAD"],
                vec!["diff", "--name-only", "-z", "HEAD"],
                vec!["diff", "--name-status", "-z", "HEAD"],
            ] {
                let expected = git(&root, &args);
                let actual = sley(&root, &args);
                assert_eq!(
                    actual, expected,
                    "sley output differed for {args:?} with path {path:?}"
                );
            }

            git(&root, &["add", path]);
            for args in [
                vec!["diff", "--cached", "--name-only"],
                vec!["diff", "--cached", "--name-status"],
                vec!["diff", "--cached", "--name-only", "-z"],
                vec!["diff", "--cached", "--name-status", "-z"],
                vec!["diff", "--cached", "--name-only", "HEAD"],
                vec!["diff", "--cached", "--name-status", "HEAD"],
                vec!["diff", "--cached", "--name-only", "-z", "HEAD"],
                vec!["diff", "--cached", "--name-status", "-z", "HEAD"],
                vec!["diff", "--staged", "--name-only", "HEAD"],
                vec!["diff", "--staged", "--name-status", "HEAD"],
                vec!["diff", "--staged", "--name-only", "-z", "HEAD"],
                vec!["diff", "--staged", "--name-status", "-z", "HEAD"],
            ] {
                let expected = git(&root, &args);
                let actual = sley(&root, &args);
                assert_eq!(
                    actual, expected,
                    "sley output differed for {args:?} with path {path:?}"
                );
            }
        };
        let _ = fs::remove_dir_all(&root);
    }
}

#[test]
fn diff_filter_matches_upstream_git() {
    let root = unique_temp_dir("diff-filter");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        git(&root, &["add", "delete.txt", "modify.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add\n").expect("write add fixture");
        git(&root, &["add", "add.txt"]);

        for args in [
            vec!["diff", "--diff-filter=A", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter", "M", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=D", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=AM", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=a", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=d", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=ad", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=R", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=A*", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=a*", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=R*", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=*", "--name-status", "HEAD"],
            vec!["diff", "--diff-filter=A", "--name-only", "-z", "HEAD"],
            vec!["diff", "--diff-filter=a", "--name-only", "-z", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--diff-filter=A",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--diff-filter=M",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--diff-filter=A",
                "--name-only",
                "-z",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec!["diff", "--quiet", "--diff-filter=A", "HEAD"],
            vec!["diff", "--quiet", "--diff-filter=R", "HEAD"],
            vec![
                "diff",
                "--exit-code",
                "--name-only",
                "--diff-filter=A",
                "HEAD",
            ],
            vec![
                "diff",
                "--exit-code",
                "--name-only",
                "--diff-filter=R",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--diff-filter=Z", "HEAD"],
            vec!["diff", "--name-status", "--diff-filter", "Z", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_pickaxe_matches_upstream_git() {
    let root = unique_temp_dir("diff-pickaxe");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join("dir")).expect("create nested directory");
        fs::write(root.join("change.txt"), b"needle\nkeep\n").expect("write change fixture");
        fs::write(root.join("delete.txt"), b"needle\n").expect("write delete fixture");
        fs::write(root.join("dir/nested.txt"), b"base\n").expect("write nested fixture");
        fs::write(root.join("other.txt"), b"plain\n").expect("write other fixture");
        fs::write(root.join("same-count.txt"), b"needle old\n").expect("write same fixture");
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("change.txt"), b"needle\nneedle\nkeep\n")
            .expect("modify change fixture");
        fs::write(root.join("dir/nested.txt"), b"base needle\n").expect("modify nested fixture");
        fs::write(root.join("other.txt"), b"changed\n").expect("modify other fixture");
        fs::write(root.join("same-count.txt"), b"needle changed\n").expect("modify same fixture");
        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("add.txt"), b"needle\n").expect("write add fixture");
        git(&root, &["add", "add.txt"]);

        for args in [
            vec!["diff", "--name-status", "-Sneedle", "HEAD"],
            vec!["diff", "--name-status", "-S", "needle", "HEAD"],
            vec!["diff", "--name-only", "-Sneedle", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "-Sneedle",
                "--diff-filter=M",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--pickaxe-all", "-Sneedle", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--pickaxe-all",
                "-Sneedle",
                "--diff-filter=R",
                "HEAD",
            ],
            vec!["diff", "--name-status", "--pickaxe-regex", "HEAD"],
            vec!["diff", "-R", "--name-status", "-Sneedle", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--relative=dir",
                "-Sneedle",
                "HEAD",
            ],
            vec!["diff", "--cached", "--name-status", "-Sneedle", "HEAD"],
            vec!["diff", "--name-status", "-Smissing", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec!["diff", "--name-status", "-S"],
            vec!["diff", "--name-status", "-S", "", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_find_object_matches_upstream_git() {
    let root = unique_temp_dir("diff-find-object");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join("dir")).expect("create nested directory");
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("dir/nested.txt"), b"nested\n").expect("write nested fixture");
        fs::write(root.join("replace.txt"), b"old\n").expect("write replace fixture");
        fs::write(root.join("unrelated.txt"), b"plain\n").expect("write unrelated fixture");
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        let old_replace = utf8_trimmed(git(&root, &["rev-parse", "HEAD:replace.txt"]));
        let delete_oid = utf8_trimmed(git(&root, &["rev-parse", "HEAD:delete.txt"]));
        let nested_oid = utf8_trimmed(git(&root, &["rev-parse", "HEAD:dir/nested.txt"]));

        fs::write(root.join("replace.txt"), b"new\n").expect("modify replace fixture");
        fs::write(root.join("dir/nested.txt"), b"nested changed\n").expect("modify nested fixture");
        fs::write(root.join("unrelated.txt"), b"changed\n").expect("modify unrelated fixture");
        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("add.txt"), b"add\n").expect("write add fixture");
        git(&root, &["add", "add.txt"]);
        let add_oid = utf8_trimmed(git(&root, &["hash-object", "add.txt"]));

        let output_cases = [
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                format!("--find-object={old_replace}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--find-object".to_string(),
                old_replace.clone(),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-only".to_string(),
                format!("--find-object={old_replace}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                format!("--find-object={delete_oid}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                format!("--find-object={add_oid}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--cached".to_string(),
                "--name-status".to_string(),
                format!("--find-object={add_oid}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                format!("--find-object={old_replace}"),
                format!("--find-object={delete_oid}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "-R".to_string(),
                "--name-status".to_string(),
                format!("--find-object={old_replace}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--relative=dir".to_string(),
                format!("--find-object={nested_oid}"),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--find-object=HEAD".to_string(),
                "HEAD".to_string(),
            ],
        ];
        for args in output_cases {
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let status_cases = [
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--find-object".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--find-object=".to_string(),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--find-object=1234".to_string(),
                "HEAD".to_string(),
            ],
            vec![
                "diff".to_string(),
                "--name-status".to_string(),
                "--pickaxe-all".to_string(),
                format!("--find-object={old_replace}"),
                "HEAD".to_string(),
            ],
        ];
        for args in status_cases {
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("diff-pathspecs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join("dir")).expect("create dir");
        fs::write(root.join("dir/a.txt"), b"before\n").expect("write dir fixture");
        fs::write(root.join("b.txt"), b"before\n").expect("write root fixture");
        git(&root, &["add", "dir/a.txt", "b.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("dir/a.txt"), b"after\n").expect("modify dir fixture");
        fs::write(root.join("b.txt"), b"after\n").expect("modify root fixture");
        fs::write(root.join("dir/c.txt"), b"new\n").expect("write staged fixture");
        git(&root, &["add", "dir/c.txt"]);

        for args in [
            vec!["diff", "--name-status", "HEAD", "--", "dir/a.txt"],
            vec!["diff", "--name-status", "HEAD", "dir/a.txt"],
            vec!["diff", "--name-status", "HEAD", "--", "dir"],
            vec!["diff", "--name-only", "-z", "HEAD", "--", "dir"],
            vec![
                "diff",
                "--diff-filter=A",
                "--name-status",
                "HEAD",
                "--",
                "dir",
            ],
            vec!["diff", "--name-status", "HEAD", "--", "missing"],
            vec!["diff", "--cached", "--name-status", "HEAD", "--", "dir"],
            vec!["diff", "--cached", "--name-only", "-z", "HEAD", "--", "dir"],
            vec!["diff", "--name-status", "--relative", "HEAD"],
            vec!["diff", "--name-status", "--relative=dir", "HEAD"],
            vec!["diff", "--name-only", "-z", "--relative=dir/", "HEAD"],
            vec!["diff", "--name-status", "--relative=missing", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--relative",
                "--no-relative",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let nested = root.join("dir");
        for args in [
            vec!["diff", "--name-status", "HEAD", "--", "a.txt"],
            vec!["diff", "--name-status", "HEAD", "a.txt"],
            vec!["diff", "--name-status", "HEAD", "--", "../b.txt"],
            vec!["diff", "--name-status", "HEAD", "--", "."],
            vec!["diff", "--name-only", "-z", "HEAD", "--", "."],
            vec!["diff", "--cached", "--name-status", "HEAD", "--", "."],
            vec!["diff", "--name-status", "--relative", "HEAD"],
            vec!["diff", "--name-only", "-z", "--relative", "HEAD"],
            vec!["diff", "--name-status", "--relative=dir", "HEAD"],
            vec![
                "diff",
                "--name-status",
                "--relative",
                "--no-relative",
                "HEAD",
            ],
        ] {
            let expected = git(&nested, &args);
            let actual = sley(&nested, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_renames_match_upstream_git() {
    let root = unique_temp_dir("diff-renames");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("old.txt"), b"same\n").expect("write old fixture");
        git(&root, &["add", "old.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        git(&root, &["mv", "old.txt", "new.txt"]);
        for args in [
            vec!["diff", "--cached", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-R", "-M", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M100%", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M", "-l0", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M", "-l", "1", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M", "-l-1", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-M", "-l1k", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--find-renames",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--find-renames=50%",
                "--name-status",
                "HEAD",
            ],
            vec!["diff", "--cached", "-M", "--name-only", "HEAD"],
            vec!["diff", "--cached", "-R", "-M", "--name-only", "HEAD"],
            vec!["diff", "--cached", "-M", "--name-status", "-z", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-R",
                "-M",
                "--name-status",
                "-z",
                "HEAD",
            ],
            vec!["diff", "--cached", "-M", "--name-only", "-z", "HEAD"],
            vec!["diff", "--cached", "-R", "-M", "--name-only", "-z", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-M",
                "--diff-filter=R",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--diff-filter=D",
                "--name-status",
                "HEAD",
            ],
            vec!["diff", "--cached", "--no-renames", "--name-status", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec!["diff", "--cached", "-M", "-l", "HEAD", "--name-status"],
            vec!["diff", "--cached", "-M", "-lfoo", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-Mfoo", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--find-renames=foo",
                "--name-status",
                "HEAD",
            ],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }

        for args in [
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-status",
                "HEAD",
                "--",
                "new.txt",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-status",
                "HEAD",
                "--",
                "old.txt",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-status",
                "HEAD",
                "--",
                "old.txt",
                "new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_relative_renames_match_upstream_git() {
    let root = unique_temp_dir("diff-relative-renames");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join("dir")).expect("create dir");
        fs::create_dir_all(root.join("other")).expect("create other dir");
        fs::write(root.join("dir/old.txt"), b"one\n").expect("write old fixture");
        fs::write(root.join("other/old.txt"), b"two\n").expect("write other fixture");
        git(&root, &["add", "dir/old.txt", "other/old.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        git(&root, &["mv", "dir/old.txt", "dir/new.txt"]);
        git(&root, &["mv", "other/old.txt", "dir/from-other.txt"]);

        for args in [
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-status",
                "--relative=dir",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-R",
                "-M",
                "--name-status",
                "--relative=dir",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-only",
                "-z",
                "--relative=dir",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-R",
                "-M",
                "--name-only",
                "-z",
                "--relative=dir",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_copies_match_upstream_git() {
    let root = unique_temp_dir("diff-copies");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("old.txt"), b"same\n").expect("write old fixture");
        git(&root, &["add", "old.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::copy(root.join("old.txt"), root.join("copy.txt")).expect("copy fixture");
        git(&root, &["add", "copy.txt"]);
        for args in [
            vec!["diff", "--cached", "-C", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-C", "-l1", "--name-status", "HEAD"],
            vec!["diff", "--cached", "-R", "-C", "--name-status", "HEAD"],
            vec!["diff", "--cached", "--find-copies", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--no-find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--no-find-copies-harder",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--find-copies-harder",
                "--no-find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--find-copies=50%",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-only",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "-z",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-only",
                "-z",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--diff-filter=C",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--diff-filter=A",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--no-renames",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
                "--",
                "copy.txt",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
                "--",
                "old.txt",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
                "--",
                "old.txt",
                "copy.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec!["diff", "--cached", "-Cfoo", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--find-copies=foo",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--find-copies-harder=1",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--no-find-copies-harder=1",
                "HEAD",
            ],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }

        fs::write(root.join("old.txt"), b"changed\n").expect("modify source fixture");
        git(&root, &["add", "old.txt"]);
        for args in [
            vec!["diff", "--cached", "-C", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-C",
                "--diff-filter=C",
                "--name-status",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_empty_rename_controls_match_upstream_git() {
    let root = unique_temp_dir("diff-empty-renames");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("old-empty.txt"), b"").expect("write empty fixture");
        git(&root, &["add", "old-empty.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        git(&root, &["mv", "old-empty.txt", "new-empty.txt"]);
        for args in [
            vec!["diff", "--cached", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--rename-empty",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--no-rename-empty",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--name-only",
                "--no-rename-empty",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        for args in [
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--rename-empty=1",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--no-rename-empty=1",
                "HEAD",
            ],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_empty_copy_controls_match_upstream_git() {
    let root = unique_temp_dir("diff-empty-copies");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("source-empty.txt"), b"").expect("write empty fixture");
        git(&root, &["add", "source-empty.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::copy(root.join("source-empty.txt"), root.join("copy-empty.txt"))
            .expect("copy empty fixture");
        git(&root, &["add", "copy-empty.txt"]);
        for args in [
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "--rename-empty",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-status",
                "--no-rename-empty",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--name-only",
                "--no-rename-empty",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_summary_matches_upstream_git() {
    let root = unique_temp_dir("diff-summary");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        fs::write(root.join("script.sh"), b"#!/bin/sh\n").expect("write mode fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
                "script.sh",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        git(&root, &["add", "-A"]);
        git(&root, &["update-index", "--chmod=+x", "script.sh"]);

        for args in [
            vec!["diff", "--cached", "--summary", "HEAD"],
            vec!["diff", "--cached", "--raw", "--summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--raw", "HEAD"],
            vec!["diff", "--cached", "--numstat", "--summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--numstat", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "--summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--stat", "--summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--stat", "HEAD"],
            vec!["diff", "--cached", "--compact-summary", "--summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--compact-summary", "HEAD"],
            vec!["diff", "--cached", "--summary", "--diff-filter=M", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--summary",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--summary",
                "--diff-filter=C",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--summary",
                "--",
                "copy-source.txt",
                "copy-dest.txt",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--summary",
                "-z",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-C",
                "--find-copies-harder",
                "--summary",
                "--name-status",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_raw_matches_upstream_git() {
    let root = unique_temp_dir("diff-raw");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        git(&root, &["add", "-A"]);

        for args in [
            vec!["diff", "--raw"],
            vec!["diff", "--raw", "HEAD"],
            vec!["diff", "--cached", "--raw", "HEAD"],
            vec!["diff", "--cached", "--raw", "-z", "HEAD"],
            vec!["diff", "--cached", "--raw", "--no-renames", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--raw",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "-C",
                "--find-copies-harder",
                "--diff-filter=C",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "-C",
                "--find-copies-harder",
                "HEAD",
                "--",
                "rename-old.txt",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "-C",
                "--find-copies-harder",
                "HEAD",
                "--",
                "rename-old.txt",
                "rename-new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_raw_abbrev_matches_upstream_git() {
    let root = unique_temp_dir("diff-raw-abbrev");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("file.txt"), b"before\n").expect("write fixture");
        git(&root, &["add", "file.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("file.txt"), b"after\n").expect("modify fixture");
        for args in [
            vec!["diff", "--raw", "HEAD"],
            vec!["diff", "--raw", "--abbrev", "HEAD"],
            vec!["diff", "--raw", "--abbrev=12", "HEAD"],
            vec!["diff", "--raw", "--abbrev=1", "HEAD"],
            vec!["diff", "--raw", "--no-abbrev", "HEAD"],
            vec!["diff", "--raw", "--full-index", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        git(&root, &["config", "core.abbrev", "12"]);
        let args = vec!["diff", "--raw", "HEAD"];
        let expected = git(&root, &args);
        let actual = sley(&root, &args);
        assert_eq!(
            actual, expected,
            "sley output differed for core.abbrev-driven raw diff"
        );
        git(&root, &["config", "--unset", "core.abbrev"]);

        git(&root, &["add", "file.txt"]);
        for args in [
            vec!["diff", "--cached", "--raw", "--abbrev=12", "HEAD"],
            vec!["diff", "--cached", "--raw", "--abbrev=1", "HEAD"],
            vec!["diff", "--cached", "--raw", "--no-abbrev", "HEAD"],
            vec!["diff", "--cached", "--raw", "--full-index", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_matches_upstream_git_for_simple_text_changes() {
    let root = unique_temp_dir("diff-patch-text");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("modify.txt"), b"one\ntwo\nthree\n").expect("write modify fixture");
        fs::write(root.join("delete.txt"), b"delete-one\ndelete-two\n")
            .expect("write delete fixture");
        git(&root, &["add", "modify.txt", "delete.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\n").expect("modify fixture");
        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("add.txt"), b"add-one\nadd-two\n").expect("write add fixture");
        git(&root, &["add", "add.txt"]);

        for args in [
            vec!["diff"],
            vec!["diff", "-p"],
            vec!["diff", "-u"],
            vec!["diff", "--patch"],
            vec!["diff", "--no-patch", "HEAD"],
            vec!["diff", "-s", "HEAD"],
            vec!["diff", "HEAD"],
            vec!["diff", "-p", "HEAD"],
            vec!["diff", "-p", "--no-patch", "HEAD"],
            vec!["diff", "--no-patch", "-p", "HEAD"],
            vec!["diff", "--raw", "-p", "HEAD"],
            vec!["diff", "--patch-with-raw", "HEAD"],
            vec!["diff", "--no-patch", "--patch-with-raw", "HEAD"],
            vec!["diff", "--patch-with-raw", "--no-patch", "HEAD"],
            vec!["diff", "--raw", "--no-patch", "HEAD"],
            vec!["diff", "--no-patch", "--raw", "HEAD"],
            vec!["diff", "--stat", "-p", "HEAD"],
            vec!["diff", "--patch-with-stat", "HEAD"],
            vec!["diff", "--no-patch", "--patch-with-stat", "HEAD"],
            vec!["diff", "--patch-with-stat", "--no-patch", "HEAD"],
            vec!["diff", "--stat", "--no-patch", "HEAD"],
            vec!["diff", "--no-patch", "--stat", "HEAD"],
            vec!["diff", "--numstat", "-p", "HEAD"],
            vec!["diff", "--numstat", "--no-patch", "HEAD"],
            vec!["diff", "--no-patch", "--numstat", "HEAD"],
            vec!["diff", "--shortstat", "-p", "HEAD"],
            vec!["diff", "--summary", "-p", "HEAD"],
            vec!["diff", "--name-only", "-p", "HEAD"],
            vec!["diff", "--name-only", "--patch-with-raw", "HEAD"],
            vec!["diff", "--patch-with-raw", "--name-only", "HEAD"],
            vec!["diff", "--name-only", "--no-patch", "HEAD"],
            vec!["diff", "--abbrev", "HEAD"],
            vec!["diff", "--abbrev=12", "HEAD"],
            vec!["diff", "--abbrev=1", "HEAD"],
            vec!["diff", "--no-abbrev", "HEAD"],
            vec!["diff", "--full-index", "HEAD"],
            vec!["diff", "--cached", "HEAD"],
            vec!["diff", "--cached", "-p", "HEAD"],
            vec!["diff", "--cached", "--patch-with-raw", "HEAD"],
            vec!["diff", "--cached", "--patch-with-stat", "HEAD"],
            vec!["diff", "--cached", "--no-patch", "HEAD"],
            vec!["diff", "--cached", "--no-prefix", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--no-prefix",
                "--default-prefix",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--default-prefix",
                "--no-prefix",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--src-prefix=old/",
                "--dst-prefix=new/",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--src-prefix",
                "old/",
                "--dst-prefix",
                "new/",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--src-prefix=old/",
                "--dst-prefix=new/",
                "--default-prefix",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        git(&root, &["config", "core.abbrev", "12"]);
        for args in [
            vec!["diff", "HEAD"],
            vec!["diff", "--no-abbrev", "HEAD"],
            vec!["diff", "--full-index", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(
                actual, expected,
                "sley output differed for core.abbrev-driven {args:?}"
            );
        }
        git(&root, &["config", "--unset", "core.abbrev"]);

        let expected = run_status(sley_testkit::oracle_git(), &root, &["diff", "--exit-code"]);
        let actual = run_status(sley_testkit::sley_bin!(), &root, &["diff", "--exit-code"]);
        assert_eq!(
            actual, expected,
            "sley result differed for diff --exit-code"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_hunk_ranges_for_single_line_changes_match_upstream_git() {
    let root = unique_temp_dir("diff-patch-single-line-ranges");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("modify.txt"), b"before\n").expect("write modify fixture");
        fs::write(root.join("delete.txt"), b"gone\n").expect("write delete fixture");
        git(&root, &["add", "modify.txt", "delete.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("add.txt"), b"new\n").expect("write add fixture");
        git(&root, &["add", "add.txt"]);

        for args in [
            vec!["diff", "HEAD"],
            vec!["diff", "--cached", "HEAD"],
            vec!["diff", "--cached", "--full-index", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_interhunk_context_config_matches_upstream_git() {
    let root = unique_temp_dir("diff-interhunk-context-config");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("file"), b"A\n1\nB\n").expect("write base fixture");
        git(&root, &["add", "file"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );
        fs::write(root.join("file"), b"X\n1\nY\n").expect("write changed fixture");

        git(&root, &["config", "diff.interHunkContext", "1"]);
        for args in [
            vec!["diff", "-U0", "file"],
            vec!["diff", "-U0", "--inter-hunk-context=0", "file"],
        ] {
            assert_eq!(
                sley(&root, &args),
                git(&root, &args),
                "sley output differed for {args:?}"
            );
        }

        let old_oid = utf8_trimmed(git(&root, &["rev-parse", "HEAD:file"]));
        let new_oid = utf8_trimmed(git(&root, &["hash-object", "-w", "file"]));
        let blob_args = ["diff", "-U0", old_oid.as_str(), new_oid.as_str()];
        assert_eq!(
            sley(&root, &blob_args),
            git(&root, &blob_args),
            "configured inter-hunk context differed for direct blobs"
        );

        for invalid in ["invalid", "-1", "2147483648", "999999999999999999999999"] {
            git(&root, &["config", "diff.interHunkContext", invalid]);
            let expected = run_status(sley_testkit::oracle_git(), &root, &["diff"]);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &["diff"]);
            assert_eq!(actual, expected, "diagnostic differed for {invalid:?}");
        }

        git(&root, &["config", "--unset", "diff.interHunkContext"]);
        for invalid in ["invalid", "-1", "2147483648"] {
            let config = format!("diff.interHunkContext={invalid}");
            let args = ["-c", config.as_str(), "diff"];
            assert_eq!(
                run_status(sley_testkit::sley_bin!(), &root, &args),
                run_status(sley_testkit::oracle_git(), &root, &args),
                "command-line diagnostic differed for {invalid:?}"
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_direct_blob_pairs_match_upstream_git() {
    let root = unique_temp_dir("diff-direct-blob-pairs");
    fs::create_dir_all(&root).expect("create temp repo");
    git(&root, &["init", "-q", "-b", "main"]);

    fs::write(root.join("old.txt"), b"old\n").expect("write old blob");
    fs::write(root.join("new.txt"), b"new\n").expect("write new blob");
    fs::write(root.join("old.bin"), b"old\0binary\n").expect("write old binary blob");
    fs::write(root.join("new.bin"), b"new\0binary\n").expect("write new binary blob");
    let old = utf8_trimmed(git(&root, &["hash-object", "-w", "old.txt"]));
    let new = utf8_trimmed(git(&root, &["hash-object", "-w", "new.txt"]));
    let old_binary = utf8_trimmed(git(&root, &["hash-object", "-w", "old.bin"]));
    let new_binary = utf8_trimmed(git(&root, &["hash-object", "-w", "new.bin"]));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("new.txt"), fs::Permissions::from_mode(0o755))
            .expect("make worktree file executable");
    }

    let cases = [
        vec!["diff", old.as_str(), new.as_str()],
        vec!["diff", "-R", old.as_str(), new.as_str()],
        vec!["diff", old_binary.as_str(), new_binary.as_str()],
        vec!["diff", old.as_str(), "new.txt"],
        vec!["diff", "new.txt", old.as_str()],
        vec!["diff", "-R", "new.txt", old.as_str()],
    ];
    for args in cases {
        assert_eq!(
            run_status(sley_testkit::sley_bin!(), &root, &args),
            run_status(sley_testkit::oracle_git(), &root, &args),
            "direct blob output differed for {args:?}"
        );
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("new.txt", root.join("link")).expect("create symlink");
        for args in [
            vec!["diff", old.as_str(), "link"],
            vec!["diff", "link", old.as_str()],
        ] {
            assert_eq!(
                run_status(sley_testkit::sley_bin!(), &root, &args),
                run_status(sley_testkit::oracle_git(), &root, &args),
                "direct blob/symlink output differed for {args:?}"
            );
        }
    }

    fs::create_dir(root.join("directory")).expect("create directory fixture");
    let args = ["diff", old.as_str(), "directory"];
    assert_eq!(
        run_status(sley_testkit::sley_bin!(), &root, &args),
        run_status(sley_testkit::oracle_git(), &root, &args),
        "non-file diagnostic differed"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_mode_changes_match_upstream_git() {
    let root = unique_temp_dir("diff-patch-mode-changes");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("script.sh"), b"#!/bin/sh\necho old\n").expect("write fixture");
        git(&root, &["add", "script.sh"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        git(&root, &["update-index", "--chmod=+x", "script.sh"]);
        for args in [
            vec!["diff", "--cached", "HEAD"],
            vec!["diff", "--cached", "--full-index", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(
                actual, expected,
                "sley output differed for mode-only {args:?}"
            );
        }

        git(&root, &["reset", "-q", "HEAD"]);
        fs::write(root.join("script.sh"), b"#!/bin/sh\necho new\n").expect("modify fixture");
        git(&root, &["add", "script.sh"]);
        git(&root, &["update-index", "--chmod=+x", "script.sh"]);
        for args in [
            vec!["diff", "--cached", "HEAD"],
            vec!["diff", "--cached", "--abbrev=12", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(
                actual, expected,
                "sley output differed for mode-and-content {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_binary_files_match_upstream_git() {
    let root = unique_temp_dir("diff-patch-binary");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("bin-mod.dat"), b"old\0bin\n").expect("write binary modify fixture");
        fs::write(root.join("bin-del.dat"), b"gone\0bin\n").expect("write binary delete fixture");
        fs::write(root.join("bin-mode.dat"), b"same\0bin\n").expect("write binary mode fixture");
        git(
            &root,
            &["add", "bin-mod.dat", "bin-del.dat", "bin-mode.dat"],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("bin-mod.dat"), b"new\0bin\nmore\0bin\n")
            .expect("modify binary fixture");
        fs::remove_file(root.join("bin-del.dat")).expect("remove binary fixture");
        fs::write(root.join("bin-add.dat"), b"add\0bin\n").expect("write binary add fixture");
        git(&root, &["add", "bin-add.dat"]);

        for args in [
            vec!["diff", "HEAD"],
            vec!["diff", "--full-index", "HEAD"],
            vec!["diff", "--src-prefix=old/", "--dst-prefix=new/", "HEAD"],
            vec!["diff", "--no-prefix", "--default-prefix", "HEAD"],
            vec!["diff", "--cached", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let mode_root = root.join("mode-only");
        fs::create_dir_all(&mode_root).expect("create mode-only repo");
        git(&mode_root, &["init", "-q", "-b", "main"]);
        fs::write(mode_root.join("bin-mode.dat"), b"same\0bin\n")
            .expect("write binary mode-only fixture");
        git(&mode_root, &["add", "bin-mode.dat"]);
        git(
            &mode_root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );
        git(&mode_root, &["update-index", "--chmod=+x", "bin-mode.dat"]);
        let args = vec!["diff", "--cached", "HEAD"];
        let expected = git(&mode_root, &args);
        let actual = sley(&mode_root, &args);
        assert_eq!(
            actual, expected,
            "sley output differed for binary mode-only {args:?}"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_renames_and_copies_match_upstream_git() {
    let root = unique_temp_dir("diff-patch-renames-copies");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("old.txt"), b"same\n").expect("write rename fixture");
        fs::write(root.join("source.txt"), b"copy\n").expect("write copy source");
        fs::write(root.join("mode-old.sh"), b"mode\n").expect("write mode rename fixture");
        git(&root, &["add", "old.txt", "source.txt", "mode-old.sh"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        git(&root, &["mv", "old.txt", "new.txt"]);
        git(&root, &["mv", "mode-old.sh", "mode-new.sh"]);
        git(&root, &["update-index", "--chmod=+x", "mode-new.sh"]);
        fs::copy(root.join("source.txt"), root.join("copy.txt")).expect("copy fixture");
        git(&root, &["add", "copy.txt"]);

        for args in [
            vec!["diff", "--cached", "-M", "HEAD"],
            vec!["diff", "--cached", "-C", "--find-copies-harder", "HEAD"],
            vec!["diff", "--cached", "-M", "--no-prefix", "HEAD"],
            vec![
                "diff",
                "--cached",
                "-M",
                "--no-prefix",
                "--default-prefix",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "-M",
                "--src-prefix=old/",
                "--dst-prefix=new/",
                "HEAD",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_patch_quoted_paths_match_upstream_git() {
    let root = unique_temp_dir("diff-patch-quoted-paths");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("space name.txt"), b"before\n").expect("write space fixture");
        fs::write(root.join("quote\"name.txt"), b"before\n").expect("write quote fixture");
        fs::write(root.join("tab\tname.txt"), b"before\n").expect("write tab fixture");
        fs::write(root.join("space bin.dat"), b"old\0bin\n").expect("write space binary fixture");
        fs::write(root.join("tab\tbin.dat"), b"old\0bin\n").expect("write tab binary fixture");
        git(
            &root,
            &[
                "add",
                "space name.txt",
                "quote\"name.txt",
                "tab\tname.txt",
                "space bin.dat",
                "tab\tbin.dat",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("space name.txt"), b"after\n").expect("modify space fixture");
        fs::write(root.join("quote\"name.txt"), b"after\n").expect("modify quote fixture");
        fs::write(root.join("tab\tname.txt"), b"after\n").expect("modify tab fixture");
        fs::write(root.join("space bin.dat"), b"new\0bin\n").expect("modify space binary fixture");
        fs::write(root.join("tab\tbin.dat"), b"new\0bin\n").expect("modify tab binary fixture");

        for args in [vec!["diff", "HEAD"], vec!["diff", "--no-prefix", "HEAD"]] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        let rename_root = root.join("rename");
        fs::create_dir_all(&rename_root).expect("create rename repo");
        git(&rename_root, &["init", "-q", "-b", "main"]);
        fs::write(rename_root.join("quote\"old.txt"), b"same\n").expect("write rename fixture");
        git(&rename_root, &["add", "quote\"old.txt"]);
        git(
            &rename_root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );
        git(&rename_root, &["mv", "quote\"old.txt", "tab\trenamed.txt"]);
        for args in [
            vec!["diff", "-M", "HEAD"],
            vec!["diff", "-M", "--no-prefix", "HEAD"],
        ] {
            let expected = git(&rename_root, &args);
            let actual = sley(&rename_root, &args);
            assert_eq!(actual, expected, "sley rename output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_numstat_matches_upstream_git() {
    let root = unique_temp_dir("diff-numstat");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete-one\ndelete-two\n")
            .expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"one\ntwo\nthree\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add-one\nadd-two\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        git(&root, &["add", "-A"]);

        for args in [
            vec!["diff", "--numstat"],
            vec!["diff", "--numstat", "HEAD"],
            vec!["diff", "--cached", "--numstat", "HEAD"],
            vec!["diff", "--cached", "--numstat", "-z", "HEAD"],
            vec!["diff", "--cached", "--numstat", "--no-renames", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--numstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--numstat",
                "-C",
                "--find-copies-harder",
                "--diff-filter=C",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "--numstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--numstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
                "--",
                "rename-old.txt",
                "rename-new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\nfive\n")
            .expect("modify unstaged fixture");
        for args in [
            vec!["diff", "--numstat"],
            vec!["diff", "--numstat", "HEAD"],
            vec!["diff", "--raw", "--numstat", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_shortstat_matches_upstream_git() {
    let root = unique_temp_dir("diff-shortstat");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete-one\ndelete-two\n")
            .expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"one\ntwo\nthree\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add-one\nadd-two\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        git(&root, &["add", "-A"]);

        for args in [
            vec!["diff", "--shortstat"],
            vec!["diff", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "-z", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "--no-renames", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--shortstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--shortstat",
                "-C",
                "--find-copies-harder",
                "--diff-filter=C",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "--shortstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--numstat",
                "--shortstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec!["diff", "--cached", "--summary", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--name-status", "--shortstat", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--shortstat",
                "-C",
                "--find-copies-harder",
                "HEAD",
                "--",
                "rename-old.txt",
                "rename-new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\nfive\n")
            .expect("modify unstaged fixture");
        for args in [
            vec!["diff", "--shortstat"],
            vec!["diff", "--shortstat", "HEAD"],
            vec!["diff", "--raw", "--shortstat", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_numstat_and_shortstat_binary_files_match_upstream_git() {
    let root = unique_temp_dir("diff-binary-stats");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("bin-mod.dat"), b"a\0b\n").expect("write binary modify fixture");
        fs::write(root.join("bin-del.dat"), b"x\0y\n").expect("write binary delete fixture");
        git(&root, &["add", "bin-mod.dat", "bin-del.dat"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("bin-mod.dat"), b"c\0d\ne\0f\n").expect("modify binary fixture");
        fs::remove_file(root.join("bin-del.dat")).expect("remove binary fixture");
        fs::write(root.join("bin-add.dat"), b"n\0m\n").expect("write binary add fixture");
        fs::write(root.join("text.txt"), b"one\n").expect("write text fixture");
        git(&root, &["add", "-A"]);

        for args in [
            vec!["diff", "--cached", "--numstat", "HEAD"],
            vec!["diff", "--cached", "--numstat", "-z", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--raw", "--numstat", "HEAD"],
            vec!["diff", "--cached", "--raw", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--numstat", "--text", "HEAD"],
            vec!["diff", "--cached", "--shortstat", "-a", "HEAD"],
            vec!["diff", "--cached", "--stat", "--text", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_stat_matches_upstream_git() {
    let root = unique_temp_dir("diff-stat");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete-one\ndelete-two\n")
            .expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"one\ntwo\nthree\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        fs::write(root.join("bin-mod.dat"), b"a\0b\n").expect("write binary modify fixture");
        fs::write(root.join("bin-del.dat"), b"x\0y\n").expect("write binary delete fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
                "bin-mod.dat",
                "bin-del.dat",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add-one\nadd-two\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        fs::write(root.join("bin-mod.dat"), b"c\0d\ne\0f\n").expect("modify binary fixture");
        fs::remove_file(root.join("bin-del.dat")).expect("remove binary fixture");
        fs::write(root.join("bin-add.dat"), b"n\0m\n").expect("write binary add fixture");
        git(&root, &["add", "-A"]);

        for args in [
            vec!["diff", "--stat"],
            vec!["diff", "--stat", "HEAD"],
            vec!["diff", "--cached", "--stat", "HEAD"],
            vec!["diff", "--cached", "--stat", "-z", "HEAD"],
            vec!["diff", "--cached", "--stat", "--no-color", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color=always", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color=never", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color=auto", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color-moved", "HEAD"],
            vec!["diff", "--cached", "--stat", "--color-moved=plain", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--stat",
                "--color-moved-ws=ignore-all-space",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "--color=always",
                "HEAD",
            ],
            vec!["diff", "--cached", "--shortstat", "--color=always", "HEAD"],
            vec!["diff", "--cached", "--stat=120,80,20", "HEAD"],
            vec!["diff", "--cached", "--stat-width=120", "HEAD"],
            vec!["diff", "--cached", "--stat-name-width=80", "HEAD"],
            vec!["diff", "--cached", "--stat-graph-width=80", "HEAD"],
            vec!["diff", "--cached", "--stat-count=20", "HEAD"],
            vec!["diff", "--cached", "--stat-count=2", "HEAD"],
            vec!["diff", "--cached", "--stat=120,80,2", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "--stat-count=2",
                "HEAD",
            ],
            vec!["diff", "--cached", "--shortstat", "--stat-count=2", "HEAD"],
            vec!["diff", "--cached", "--stat", "--no-renames", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--stat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--stat",
                "-C",
                "--find-copies-harder",
                "--diff-filter=C",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--raw",
                "--stat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--numstat",
                "--stat",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec!["diff", "--cached", "--stat", "--summary", "HEAD"],
            vec!["diff", "--cached", "--stat", "--shortstat", "HEAD"],
            vec!["diff", "--cached", "--stat", "--name-status", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--stat",
                "-C",
                "--find-copies-harder",
                "HEAD",
                "--",
                "rename-old.txt",
                "rename-new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }

        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\nfour\nfive\n")
            .expect("modify unstaged fixture");
        fs::write(root.join("bin-mod.dat"), b"later\0binary\n").expect("modify binary unstaged");
        for args in [
            vec!["diff", "--stat"],
            vec!["diff", "--stat", "HEAD"],
            vec!["diff", "--raw", "--stat", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_stat_excludes_unmerged_rows_from_totals() {
    let root = unique_temp_dir("diff-stat-unmerged-total");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        for path in ["a", "b", "c", "d"] {
            fs::write(root.join(path), b"").expect("write empty fixture");
        }
        git(&root, &["add", "a", "b", "c", "d"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("a"), b"a\n").expect("modify a");
        fs::write(root.join("b"), b"b\n").expect("modify b");
        let stage0 =
            String::from_utf8(git(&root, &["ls-files", "-s", "a"])).expect("stage output is utf8");
        git(&root, &["rm", "-f", "d"]);
        let mut index_info = String::new();
        for stage in 1..=3 {
            index_info.push_str(&stage0.replace(" 0\ta", &format!(" {stage}\td")));
        }
        let mut child = Command::new(sley_testkit::oracle_git())
            .current_dir(&root)
            .args(["update-index", "--index-info"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn update-index");
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(index_info.as_bytes())
            .expect("write index-info");
        let status = child.wait().expect("wait for update-index");
        assert!(status.success(), "update-index failed with {status:?}");
        fs::write(root.join("d"), b"d\n").expect("write unmerged worktree file");

        for args in [
            vec!["diff", "--stat"],
            vec!["diff", "--stat", "--stat-count=2"],
            vec!["diff", "--numstat"],
            vec!["diff", "--shortstat"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_compact_summary_matches_upstream_git() {
    let root = unique_temp_dir("diff-compact-summary");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("modify.txt"), b"one\ntwo\n").expect("write modify fixture");
        fs::write(root.join("rename-old.txt"), b"rename\n").expect("write rename fixture");
        fs::write(root.join("copy-source.txt"), b"copy\n").expect("write copy fixture");
        fs::write(root.join("script.sh"), b"#!/bin/sh\n").expect("write mode fixture");
        git(
            &root,
            &[
                "add",
                "delete.txt",
                "modify.txt",
                "rename-old.txt",
                "copy-source.txt",
                "script.sh",
            ],
        );
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::remove_file(root.join("delete.txt")).expect("remove delete fixture");
        fs::write(root.join("modify.txt"), b"one\nTWO\nthree\n").expect("modify fixture");
        fs::write(root.join("add.txt"), b"add\n").expect("write add fixture");
        git(&root, &["mv", "rename-old.txt", "rename-new.txt"]);
        fs::copy(root.join("copy-source.txt"), root.join("copy-dest.txt")).expect("copy fixture");
        git(&root, &["add", "-A"]);
        git(&root, &["update-index", "--chmod=+x", "script.sh"]);

        for args in [
            vec!["diff", "--cached", "--compact-summary", "HEAD"],
            vec!["diff", "--cached", "--stat", "--compact-summary", "HEAD"],
            vec!["diff", "--cached", "--compact-summary", "--summary", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "-C",
                "--find-copies-harder",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "--no-renames",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "--diff-filter=M",
                "HEAD",
            ],
            vec!["diff", "--cached", "--raw", "--compact-summary", "HEAD"],
            vec!["diff", "--cached", "--numstat", "--compact-summary", "HEAD"],
            vec![
                "diff",
                "--cached",
                "--name-status",
                "--compact-summary",
                "HEAD",
            ],
            vec![
                "diff",
                "--cached",
                "--compact-summary",
                "HEAD",
                "--",
                "rename-old.txt",
                "rename-new.txt",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_exit_code_and_quiet_match_upstream_git() {
    let root = unique_temp_dir("diff-exit-code-quiet");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("modify.txt"), b"before\n").expect("write fixture");
        git(&root, &["add", "modify.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("modify.txt"), b"after\n").expect("modify fixture");
        for args in [
            vec!["diff", "--exit-code", "--name-only"],
            vec!["diff", "--quiet"],
            vec!["diff", "--exit-code", "--name-only", "HEAD"],
            vec!["diff", "--name-status", "--exit-code", "HEAD"],
            vec!["diff", "--quiet", "HEAD"],
            vec!["diff", "--quiet", "--name-only", "HEAD"],
            vec!["diff", "--exit-code", "--no-patch", "HEAD"],
            vec!["diff", "--quiet", "--no-patch", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }

        git(&root, &["add", "modify.txt"]);
        for args in [
            vec!["diff", "--cached", "--exit-code", "--name-only"],
            vec!["diff", "--staged", "--quiet"],
            vec!["diff", "--cached", "--exit-code", "--name-only", "HEAD"],
            vec!["diff", "--staged", "--quiet", "HEAD"],
            vec!["diff", "--cached", "--exit-code", "--no-patch", "HEAD"],
            vec!["diff", "--staged", "--quiet", "--no-patch", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }

        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "clean",
                "-q",
            ],
        );
        for args in [
            vec!["diff", "--exit-code", "--name-only"],
            vec!["diff", "--quiet"],
            vec!["diff", "--exit-code", "--name-only", "HEAD"],
            vec!["diff", "--quiet", "HEAD"],
            vec!["diff", "--cached", "--quiet", "HEAD"],
            vec!["diff", "--exit-code", "--no-patch", "HEAD"],
            vec!["diff", "--cached", "--quiet", "--no-patch", "HEAD"],
        ] {
            let expected = run_status(sley_testkit::oracle_git(), &root, &args);
            let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
            assert_eq!(actual, expected, "sley result differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_between_revisions_matches_upstream_git() {
    // Regression: `diff <rev> <rev>` (and the single-rev / range forms) used to be
    // mis-parsed as pathspecs and silently fell back to an index-vs-worktree diff.
    let root = unique_temp_dir("diff-between-revisions");
    fs::create_dir_all(&root).expect("create repo root");
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"line1\nline2\n").expect("write a");
    fs::create_dir_all(root.join("sub")).expect("mkdir sub");
    fs::write(root.join("sub/b.txt"), b"x\n").expect("write b");
    git(&root, &["add", "-A"]);
    let commit = |msg: &str| {
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                msg,
                "-q",
            ],
        );
    };
    commit("c1");
    fs::write(root.join("a.txt"), b"line1\nCHANGED\nline2\n").expect("modify a");
    fs::write(root.join("sub/b.txt"), b"y\n").expect("modify b");
    fs::write(root.join("c.txt"), b"added\n").expect("write c");
    git(&root, &["add", "-A"]);
    commit("c2");

    // Worktree is clean (everything committed), so a bare `diff` is empty while the
    // revision forms must report the c1->c2 changes — exactly git's behavior.
    for args in [
        vec!["diff", "--name-status", "HEAD~1", "HEAD"],
        vec!["diff", "HEAD~1", "HEAD"],
        vec!["diff", "HEAD~1..HEAD"],
        vec!["diff", "HEAD~1...HEAD"],
        vec!["diff", "--name-status", "HEAD~1"],
        vec!["diff", "--cached", "HEAD~1"],
        vec!["diff", "--stat", "HEAD~1", "HEAD"],
        vec!["diff", "--raw", "HEAD~1", "HEAD"],
        vec!["diff", "--numstat", "HEAD~1", "HEAD"],
        vec!["diff", "HEAD~1", "HEAD", "--", "a.txt"],
        vec!["diff", "--name-status", "HEAD~1", "HEAD", "--", "sub"],
        vec!["diff"],
    ] {
        assert_eq!(
            sley(&root, &args),
            git(&root, &args),
            "sley diff output diverged from git for {args:?}",
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_two_tree_uses_committed_content_not_dirty_worktree() {
    // Regressions: (1) `diff A B` patch/content must come from B's blobs, not the
    // worktree; (2) `diff HEAD HEAD` must parse as two revisions (empty), not
    // consume the first HEAD as the head-vs-worktree shortcut.
    let root = unique_temp_dir("diff-two-tree-dirty");
    fs::create_dir_all(&root).expect("create repo root");
    git(&root, &["init", "-q", "-b", "main"]);
    let commit = |msg: &str| {
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                msg,
                "-q",
            ],
        );
    };
    fs::write(root.join("f.txt"), b"a\nb\n").expect("write f");
    git(&root, &["add", "-A"]);
    commit("c1");
    git(&root, &["tag", "v1.0"]);
    fs::write(root.join("f.txt"), b"a\nB2\n").expect("modify f");
    git(&root, &["add", "-A"]);
    commit("c2");
    git(&root, &["tag", "v1.1"]);
    // Leave the worktree dirty so a worktree-sourced "new" side would diverge.
    fs::write(root.join("f.txt"), b"DIRTY WORKTREE CONTENT\n").expect("dirty f");

    for args in [
        vec!["diff", "HEAD~1", "HEAD"],
        vec!["diff", "--raw", "HEAD~1", "HEAD"],
        vec!["diff", "HEAD", "HEAD"],
        vec!["diff", "HEAD~1..HEAD"],
        vec!["diff", "HEAD~1...v1.1"],
        vec!["diff", "v1.0", "v1.1"],
        vec!["diff", "HEAD~1", "HEAD", "--", "f.txt"],
    ] {
        assert_eq!(
            sley(&root, &args),
            git(&root, &args),
            "sley diff diverged from git for {args:?} (dirty worktree)",
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_outside_repository_implicitly_uses_no_index() {
    let root = unique_temp_dir("diff-implicit-no-index-outside-repository");
    fs::create_dir_all(&root).expect("create non-repository root");
    fs::write(root.join("one"), b"one\n").expect("write old side");
    fs::write(root.join("two"), b"two\n").expect("write new side");

    let run = |program: &str| {
        Command::new(program)
            .current_dir(&root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["diff", "one", "two"])
            .output()
            .unwrap_or_else(|err| panic!("failed to run {program}: {err}"))
    };
    let oracle = run(sley_testkit::oracle_git());
    let actual = run(sley_testkit::sley_bin!());
    assert_eq!(actual.status.code(), oracle.status.code());
    assert_eq!(actual.stdout, oracle.stdout);
    assert_eq!(actual.stderr, oracle.stderr);

    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn diff_no_index_external_diff_uses_repository_object_format_for_null_ids() {
    use std::os::unix::fs::PermissionsExt;

    let root = unique_temp_dir("diff-no-index-external-sha256-null-ids");
    fs::create_dir_all(&root).expect("create repository root");
    git(
        &root,
        &["init", "-q", "-b", "main", "--object-format=sha256"],
    );
    fs::write(root.join("executable"), b"content\n").expect("write executable side");
    fs::write(root.join("not-executable"), b"content\n").expect("write regular side");
    let mut permissions = fs::metadata(root.join("executable"))
        .expect("stat executable side")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(root.join("executable"), permissions).expect("chmod executable side");

    let args = [
        "-c",
        "diff.external=echo diff",
        "diff",
        "--no-index",
        "executable",
        "not-executable",
    ];
    let oracle = run_status(sley_testkit::oracle_git(), &root, &args);
    let actual = run_status(sley_testkit::sley_bin!(), &root, &args);
    assert_eq!(actual, oracle);
    assert_eq!(actual.0, 1);
    assert!(
        String::from_utf8_lossy(&actual.1).contains(&"0".repeat(64)),
        "external diff did not receive SHA-256-width null ids: {}",
        String::from_utf8_lossy(&actual.1),
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn diff_no_index_rejects_stdin_directory_without_reading_stdin() {
    let root = unique_temp_dir("diff-no-index-stdin-directory");
    fs::create_dir_all(root.join("a")).expect("create directory side");
    let output = Command::new(sley_testkit::sley_bin!())
        .current_dir(&root)
        .args(["diff", "--no-index", "--", "-", "a"])
        .stdin(Stdio::null())
        .output()
        .expect("run sley diff --no-index");
    assert!(
        !output.status.success(),
        "diff --no-index unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fatal: cannot compare stdin to a directory"),
        "unexpected stderr: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn diff_no_index_rejects_fifo_directory_without_reading_fifo() {
    let root = unique_temp_dir("diff-no-index-fifo-directory");
    fs::create_dir_all(root.join("a")).expect("create directory side");
    let status = Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .expect("mkfifo");
    assert!(status.success(), "mkfifo failed with {status:?}");

    let output = Command::new(sley_testkit::sley_bin!())
        .current_dir(&root)
        .args(["diff", "--no-index", "--", "pipe", "a"])
        .output()
        .expect("run sley diff --no-index");
    assert!(
        !output.status.success(),
        "diff --no-index unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fatal: cannot compare a named pipe to a directory"),
        "unexpected stderr: {stderr}"
    );
    let _ = fs::remove_dir_all(&root);
}
