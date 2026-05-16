use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
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

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

#[test]
fn ls_tree_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("ls-tree-pathspecs");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("--dash.txt"), b"dash\n").expect("write dash path");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["ls-tree", "HEAD", "dir"],
            vec!["ls-tree", "HEAD", "dir/"],
            vec!["ls-tree", "HEAD", "dir/b.txt"],
            vec!["ls-tree", "HEAD", "--", "dir"],
            vec!["ls-tree", "--", "HEAD", "dir"],
            vec!["ls-tree", "HEAD", "--", "--dash.txt"],
            vec!["ls-tree", "-r", "HEAD", "dir"],
            vec!["ls-tree", "-r", "HEAD", "--", "dir"],
            vec!["ls-tree", "--name-only", "HEAD", "dir/"],
            vec!["ls-tree", "--long", "HEAD", "dir/b.txt"],
            vec!["ls-tree", "-z", "HEAD", "dir/"],
            vec!["ls-tree", "HEAD", "missing"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_show_trees_matches_upstream_git() {
    let root = unique_temp_dir("ls-tree-show-trees");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["ls-tree", "-t", "HEAD"],
            vec!["ls-tree", "-t", "HEAD", "dir"],
            vec!["ls-tree", "-t", "HEAD", "dir/"],
            vec!["ls-tree", "-r", "-t", "HEAD"],
            vec!["ls-tree", "-r", "-t", "HEAD", "dir"],
            vec!["ls-tree", "-r", "-t", "HEAD", "dir/"],
            vec!["ls-tree", "-r", "-t", "-z", "HEAD"],
            vec!["ls-tree", "-r", "-t", "--name-only", "HEAD"],
            vec!["ls-tree", "-r", "-t", "--object-only", "HEAD"],
            vec!["ls-tree", "-r", "-t", "--long", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_directories_only_matches_upstream_git() {
    let root = unique_temp_dir("ls-tree-directories-only");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["ls-tree", "-d", "HEAD"],
            vec!["ls-tree", "-d", "HEAD", "dir"],
            vec!["ls-tree", "-d", "HEAD", "dir/"],
            vec!["ls-tree", "-r", "-d", "HEAD"],
            vec!["ls-tree", "-r", "-d", "HEAD", "dir"],
            vec!["ls-tree", "-r", "-d", "HEAD", "dir/"],
            vec!["ls-tree", "-d", "-z", "HEAD"],
            vec!["ls-tree", "-d", "--name-only", "HEAD", "dir/"],
            vec!["ls-tree", "-d", "--object-only", "HEAD", "dir/"],
            vec!["ls-tree", "-d", "--long", "HEAD", "dir/"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_name_status_matches_upstream_git() {
    let root = unique_temp_dir("ls-tree-name-status");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["ls-tree", "--name-status", "HEAD"],
            vec!["ls-tree", "--name-status", "-z", "HEAD"],
            vec!["ls-tree", "--name-status", "HEAD", "dir/"],
            vec!["ls-tree", "--name-status", "-r", "HEAD"],
            vec!["ls-tree", "--name-status", "-r", "-t", "HEAD"],
            vec!["ls-tree", "--name-status", "-d", "HEAD", "dir/"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_quoted_paths_match_upstream_git() {
    let root = unique_temp_dir("ls-tree-quoted-paths");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (case, path) in [
            ("space", "space name.txt"),
            ("quote", "quote\"name.txt"),
            ("tab", "tab\tname.txt"),
        ] {
            let repo = root.join(case);
            fs::create_dir_all(&repo).expect("create case repo");
            git(&repo, &["init", "-q"]);
            fs::write(repo.join(path), b"content\n").expect("write quoted fixture");
            git(&repo, &["add", path]);
            git(
                &repo,
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

            for args in [
                vec!["ls-tree", "HEAD"],
                vec!["ls-tree", "-z", "HEAD"],
                vec!["ls-tree", "--name-only", "HEAD"],
                vec!["ls-tree", "--name-only", "-z", "HEAD"],
                vec!["ls-tree", "--format=%(path)", "HEAD"],
                vec!["ls-tree", "-z", "--format=%(path)", "HEAD"],
                vec![
                    "ls-tree",
                    "--format=%(objectmode) %(objecttype) %(objectname)%x09%(path)",
                    "HEAD",
                ],
            ] {
                let expected = git(&repo, &args);
                let actual = git_rs(&repo, &args);
                assert_eq!(
                    actual, expected,
                    "git-rs output differed for {args:?} path {path:?}"
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_abbrev_matches_upstream_git() {
    let root = unique_temp_dir("ls-tree-abbrev");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec!["ls-tree", "--abbrev", "HEAD"],
            vec!["ls-tree", "--abbrev=8", "HEAD"],
            vec!["ls-tree", "--abbrev=1", "HEAD"],
            vec!["ls-tree", "--abbrev=0", "HEAD"],
            vec!["ls-tree", "--abbrev=80", "HEAD"],
            vec!["ls-tree", "--abbrev=8", "--object-only", "HEAD"],
            vec!["ls-tree", "--abbrev=8", "--long", "HEAD"],
            vec!["ls-tree", "--abbrev=8", "-r", "-t", "HEAD"],
            vec!["ls-tree", "--abbrev=8", "-d", "HEAD", "dir/"],
            vec!["ls-tree", "--abbrev=8", "--no-abbrev", "HEAD"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_nested_cwd_full_name_and_full_tree_match_upstream_git() {
    let root = unique_temp_dir("ls-tree-nested-cwd");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    fs::create_dir_all(root.join("other")).expect("create other dir");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
        fs::write(root.join("dir/sub/c.txt"), b"c\n").expect("write c");
        fs::write(root.join("other/o.txt"), b"o\n").expect("write o");
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
                "initial",
                "-q",
            ],
        );

        let nested = root.join("dir");
        for args in [
            vec!["ls-tree", "HEAD"],
            vec!["ls-tree", "-r", "HEAD"],
            vec!["ls-tree", "--full-name", "HEAD"],
            vec!["ls-tree", "--full-name", "-r", "HEAD"],
            vec!["ls-tree", "--full-name", "--no-full-name", "HEAD"],
            vec!["ls-tree", "--full-tree", "HEAD"],
            vec!["ls-tree", "--full-tree", "-r", "HEAD"],
            vec!["ls-tree", "--full-tree", "--no-full-tree", "HEAD"],
            vec!["ls-tree", "--full-tree", "--no-full-tree", "-r", "HEAD"],
            vec![
                "ls-tree",
                "--full-name",
                "--full-tree",
                "--no-full-tree",
                "HEAD",
            ],
            vec![
                "ls-tree",
                "--full-tree",
                "--full-name",
                "--no-full-tree",
                "HEAD",
            ],
            vec!["ls-tree", "HEAD", "sub"],
            vec!["ls-tree", "--full-name", "HEAD", "sub"],
            vec!["ls-tree", "HEAD", "../other"],
            vec!["ls-tree", "--full-name", "HEAD", "../other"],
            vec!["ls-tree", "--full-tree", "HEAD", "sub"],
        ] {
            let expected = git(&nested, &args);
            let actual = git_rs(&nested, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_tree_format_placeholders_match_upstream_git() {
    let root = unique_temp_dir("ls-tree-format");
    fs::create_dir_all(root.join("dir")).expect("create fixture dirs");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"a\n").expect("write a");
        fs::write(root.join("dir/b.txt"), b"b\n").expect("write b");
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
                "initial",
                "-q",
            ],
        );

        for args in [
            vec![
                "ls-tree",
                "--format=%(objectmode) %(objecttype) %(objectname)%x09%(path)",
                "HEAD",
            ],
            vec!["ls-tree", "--format=%(path)", "HEAD"],
            vec!["ls-tree", "--format=%(objectsize) %(path)", "HEAD"],
            vec!["ls-tree", "--format=%(objectsize:padded) %(path)", "HEAD"],
            vec!["ls-tree", "--format=%% %(path)", "HEAD"],
            vec!["ls-tree", "-z", "--format=%(path)", "HEAD"],
            vec!["ls-tree", "-r", "--format=%(objectname) %(path)", "HEAD"],
            vec![
                "ls-tree",
                "--abbrev=8",
                "--format=%(objectname) %(path)",
                "HEAD",
            ],
            vec!["ls-tree", "--format", "%(path)", "HEAD", "dir/"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
