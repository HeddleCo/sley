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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_with_stdin(program, cwd, args, &[])
}

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let output = run_output_with_stdin(program, cwd, args, stdin);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        stdin,
    );

    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_output_with_env_and_stdin(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("stdin pipe"),
        stdin,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
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

fn sley(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(sley_testkit::sley_bin!(), cwd, args, stdin)
}

fn git(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    run_with_stdin(sley_testkit::oracle_git(), cwd, args, stdin)
}

#[test]
fn hash_object_usage_and_option_errors_exit_like_upstream_git() {
    let root = unique_temp_dir("hash-object-usage");
    fs::create_dir_all(&root).expect("create temp root");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        let stdin = b"stdin\n";
        for args in [
            vec!["hash-object"],
            vec!["hash-object", "--stdin", "--stdin"],
            vec!["hash-object", "--stdin", "--stdin-paths"],
            vec!["hash-object", "--object-format"],
            vec!["hash-object", "--object-format="],
            vec!["hash-object", "--bogus"],
            vec!["hash-object", "-x"],
            vec!["hash-object", "-t"],
            vec!["hash-object", "--stdin=value", "--stdin"],
            vec!["hash-object", "--path"],
        ] {
            let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, stdin);
            let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, stdin);
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "sley exit status differed for {args:?}\nexpected stderr:\n{}\nactual stderr:\n{}",
                String::from_utf8_lossy(&expected.stderr),
                String::from_utf8_lossy(&actual.stderr)
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_git_object_directory_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run(
            sley_testkit::oracle_git(),
            &expected,
            &["init", "-q", "-b", "main"],
        );
        run(
            sley_testkit::oracle_git(),
            &actual,
            &["init", "-q", "-b", "main"],
        );
        for repo in [&expected, &actual] {
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom object dir");
        }
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        let write_args = ["hash-object", "-w", "--stdin"];
        let stdin = b"custom object\n";
        let expected_write = run_output_with_env_and_stdin(
            sley_testkit::oracle_git(),
            &expected,
            &write_args,
            &envs,
            stdin,
        );
        let actual_write = run_output_with_env_and_stdin(
            sley_testkit::sley_bin!(),
            &actual,
            &write_args,
            &envs,
            stdin,
        );
        assert_same_output(actual_write, expected_write, &write_args);

        let oid = String::from_utf8(
            run_output_with_env_and_stdin(
                sley_testkit::oracle_git(),
                &expected,
                &write_args,
                &envs,
                stdin,
            )
            .stdout,
        )
        .expect("oid is utf8")
        .trim()
        .to_string();
        let cat_args = ["cat-file", "-p", oid.as_str()];
        let expected_cat = run_output_with_env_and_stdin(
            sley_testkit::oracle_git(),
            &expected,
            &cat_args,
            &envs,
            &[],
        );
        let actual_cat = run_output_with_env_and_stdin(
            sley_testkit::sley_bin!(),
            &actual,
            &cat_args,
            &envs,
            &[],
        );
        assert_same_output(actual_cat, expected_cat, &cat_args);

        let (fanout, name) = oid.split_at(2);
        assert!(
            expected
                .join("custom-objects")
                .join(fanout)
                .join(name)
                .exists()
        );
        assert!(
            actual
                .join("custom-objects")
                .join(fanout)
                .join(name)
                .exists()
        );
        assert!(
            !actual
                .join(".git")
                .join("objects")
                .join(fanout)
                .join(name)
                .exists()
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_multiple_inputs_match_upstream_git() {
    let root = unique_temp_dir("hash-object-multiple-inputs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");
        fs::write(root.join("--stdin"), b"dash stdin\n").expect("write option-like path");
        fs::write(root.join("--path"), b"dash path\n").expect("write path-like path");

        let stdin = b"stdin\n";
        for args in [
            vec!["hash-object", "one.txt", "two.txt"],
            vec!["hash-object", "-tblob", "one.txt"],
            vec!["hash-object", "--stdin", "one.txt", "two.txt"],
            vec!["hash-object", "-tblob", "--stdin"],
            vec!["hash-object", "-w", "--stdin", "one.txt", "two.txt"],
            vec!["hash-object", "-w", "-tblob", "one.txt"],
            vec!["hash-object", "--", "--stdin", "--path"],
            vec!["hash-object", "--stdin", "--", "--stdin", "one.txt"],
            vec!["hash-object", "-w", "--", "--stdin"],
            vec!["hash-object", "--"],
        ] {
            let expected = git(&root, &args, stdin);
            let actual = sley(&root, &args, stdin);
            assert_eq!(actual, expected, "sley output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_sha256_repo_default_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-sha256-default");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run(
            sley_testkit::oracle_git(),
            &expected,
            &["init", "-q", "--object-format=sha256", "-b", "main"],
        );
        run(
            sley_testkit::oracle_git(),
            &actual,
            &["init", "-q", "--object-format=sha256", "-b", "main"],
        );
        for repo in [&expected, &actual] {
            fs::write(repo.join("one.txt"), b"one\n").expect("write one");
            fs::write(repo.join("two.txt"), b"two\n").expect("write two");
            fs::write(repo.join("paths.txt"), b"one.txt\ntwo.txt\n").expect("write path list");
        }

        for (args, stdin) in [
            (vec!["hash-object", "--stdin"], b"stdin\n".as_slice()),
            (vec!["hash-object", "one.txt"], b"".as_slice()),
            (
                vec!["hash-object", "--stdin", "one.txt"],
                b"stdin\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\ntwo.txt\n".as_slice(),
            ),
        ] {
            let expected_output =
                run_output_with_stdin(sley_testkit::oracle_git(), &expected, &args, stdin);
            let actual_output =
                run_output_with_stdin(sley_testkit::sley_bin!(), &actual, &args, stdin);
            assert_same_output(actual_output, expected_output, &args);
            assert!(
                String::from_utf8_lossy(&sley(&actual, &args, stdin))
                    .lines()
                    .all(|line| line.len() == 64),
                "expected SHA-256 object ids for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_filter_path_and_option_errors_match_upstream_git() {
    let root = unique_temp_dir("hash-object-filter-path");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        let stdin = b"stdin\n";

        for args in [
            vec!["hash-object", "--no-filters", "one.txt"],
            vec!["hash-object", "--filters", "one.txt"],
            vec!["hash-object", "--literally", "one.txt"],
            vec!["hash-object", "--no-literally", "one.txt"],
            vec!["hash-object", "--no-stdin", "one.txt"],
            vec!["hash-object", "--path=virtual.txt", "--stdin"],
            vec!["hash-object", "--path", "virtual.txt", "--stdin"],
            vec!["hash-object", "--no-path", "--stdin"],
            vec![
                "hash-object",
                "--path",
                "virtual.txt",
                "--no-path",
                "--stdin",
            ],
            vec![
                "hash-object",
                "--no-path",
                "--path",
                "virtual.txt",
                "--stdin",
            ],
            vec!["hash-object", "--path=virtual.txt", "--no-path", "--stdin"],
            vec!["hash-object", "--no-filters", "--stdin"],
            vec!["hash-object", "--filters", "--stdin"],
            vec!["hash-object", "--literally", "--stdin"],
            vec!["hash-object", "--no-literally", "--stdin"],
            vec!["hash-object", "--no-stdin"],
            vec!["hash-object", "-t"],
            vec!["hash-object", "--stdin=value", "--stdin"],
            vec!["hash-object", "--no-stdin=value", "--stdin"],
            vec!["hash-object", "--stdin-paths=value", "--stdin"],
            vec!["hash-object", "--no-stdin-paths=value", "--stdin"],
            vec!["hash-object", "--filters=value", "--stdin"],
            vec!["hash-object", "--no-filters=value", "--stdin"],
            vec!["hash-object", "--literally=value", "--stdin"],
            vec!["hash-object", "--no-literally=value", "--stdin"],
            vec!["hash-object", "--no-path=value", "--stdin"],
        ] {
            let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, stdin);
            let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_stdin_paths_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-stdin-paths");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");

        for (args, stdin) in [
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\ntwo.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths", "--no-filters"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--stdin-paths", "--no-stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--no-stdin-paths", "--stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (
                vec!["hash-object", "--no-stdin-paths"],
                b"one.txt\n".as_slice(),
            ),
            (vec!["hash-object", "--stdin-paths"], b"".as_slice()),
            (
                vec!["hash-object", "--stdin-paths"],
                b"one.txt\nmissing\n".as_slice(),
            ),
        ] {
            let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, stdin);
            let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_stdin_paths_parent_dir_matches_upstream_git() {
    let root = unique_temp_dir("hash-object-stdin-paths-dotdot");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::create_dir_all(root.join("dir")).expect("create dir");
        fs::write(root.join("file.txt"), b"payload\n").expect("write file");
        let args = ["hash-object", "--stdin-paths"];
        let stdin = b"dir/../file.txt\n";
        let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, stdin);
        assert!(
            expected.status.success(),
            "oracle git should hash dir/../file.txt"
        );
        let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, stdin);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn hash_object_parent_dir_through_symlink_matches_upstream_git() {
    use std::os::unix::fs::symlink;

    let root = unique_temp_dir("hash-object-symlink-dotdot");
    fs::create_dir_all(root.join("real").join("sub")).expect("create target directories");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join(".gitattributes"), b"* -text\n").expect("write root attributes");
        fs::write(
            root.join("real").join(".gitattributes"),
            b"*.txt text eol=lf\n",
        )
        .expect("write target attributes");
        fs::write(root.join("real").join("f2.txt"), b"line\r\n").expect("write target file");
        symlink("real/sub", root.join("link")).expect("create directory symlink");

        let args = ["hash-object", "link/../f2.txt"];
        let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, &[]);
        let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, &[]);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_linked_worktree_uses_common_info_attributes() {
    let root = unique_temp_dir("hash-object-linked-info-attributes");
    let main = root.join("main");
    let linked = root.join("linked");
    fs::create_dir_all(&main).expect("create main worktree");
    {
        let git = sley_testkit::oracle_git();
        run(git, &main, &["init", "-q", "-b", "main"]);
        run(
            git,
            &main,
            &[
                "-c",
                "user.name=Sley Tests",
                "-c",
                "user.email=sley@example.com",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "base",
            ],
        );
        run(
            git,
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().expect("linked path is utf-8"),
            ],
        );

        fs::create_dir_all(main.join(".git").join("info")).expect("create common info dir");
        fs::write(
            main.join(".git").join("info").join("attributes"),
            b"*.attr text eol=lf\n",
        )
        .expect("write common attributes");
        run(git, &main, &["config", "core.autocrlf", "true"]);
        let admin_dir = String::from_utf8(run(git, &linked, &["rev-parse", "--git-dir"]))
            .expect("git dir output is utf-8");
        let admin_dir = PathBuf::from(admin_dir.trim());
        fs::create_dir_all(admin_dir.join("info")).expect("create worktree admin info dir");
        fs::write(admin_dir.join("info").join("attributes"), b"*.attr -text\n")
            .expect("write ignored per-worktree attributes");
        fs::write(linked.join("file.attr"), b"attributes\r\n")
            .expect("write attribute-controlled file");
        fs::write(linked.join("config.txt"), b"config\r\n").expect("write config-controlled file");

        let args = ["hash-object", "--stdin-paths"];
        let stdin = b"file.attr\nconfig.txt\n";
        let expected = run_output_with_stdin(git, &linked, &args, stdin);
        let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &linked, &args, stdin);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_honors_git_common_dir_info_attributes() {
    let root = unique_temp_dir("hash-object-git-common-dir-attributes");
    let common = root.join("custom-common");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        let git = sley_testkit::oracle_git();
        run(git, &root, &["init", "-q", "-b", "main"]);
        for directory in ["info", "objects", "refs"] {
            fs::create_dir_all(common.join(directory)).expect("create custom common directory");
        }
        fs::write(
            common.join("info").join("attributes"),
            b"*.txt text eol=lf\n",
        )
        .expect("write custom common attributes");
        fs::create_dir_all(root.join(".git").join("info")).expect("create git info dir");
        fs::write(
            root.join(".git").join("info").join("attributes"),
            b"*.txt -text\n",
        )
        .expect("write ignored git-dir attributes");
        fs::write(root.join("file.txt"), b"line\r\n").expect("write input file");

        let git_dir = root.join(".git");
        let git_dir = git_dir.to_str().expect("git dir is utf-8");
        let common = common.to_str().expect("common dir is utf-8");
        let worktree = root.to_str().expect("worktree is utf-8");
        let envs = [
            ("GIT_DIR", git_dir),
            ("GIT_COMMON_DIR", common),
            ("GIT_WORK_TREE", worktree),
        ];
        let args = ["hash-object", "file.txt"];
        let expected = run_output_with_env_and_stdin(git, &root, &args, &envs, &[]);
        let actual =
            run_output_with_env_and_stdin(sley_testkit::sley_bin!(), &root, &args, &envs, &[]);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hash_object_stdin_paths_nested_attributes_match_upstream_git() {
    let root = unique_temp_dir("hash-object-stdin-paths-attrs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        fs::write(root.join(".gitattributes"), b"* text\n").expect("write root attributes");
        let mut stdin = Vec::new();
        for dir_index in 0..3 {
            let dir = root.join(format!("dir-{dir_index}"));
            fs::create_dir_all(&dir).expect("create attr dir");
            fs::write(dir.join(".gitattributes"), b"*.txt text eol=lf\n")
                .expect("write dir attributes");
            for file_index in 0..20 {
                let name = format!("file-{file_index:02}.txt");
                fs::write(dir.join(&name), b"line\r\nline\r\n").expect("write hashed file");
                stdin.extend_from_slice(format!("dir-{dir_index}/{name}\n").as_bytes());
            }
        }
        let args = ["hash-object", "--stdin-paths"];
        let expected = run_output_with_stdin(sley_testkit::oracle_git(), &root, &args, &stdin);
        let actual = run_output_with_stdin(sley_testkit::sley_bin!(), &root, &args, &stdin);
        assert_same_output(actual, expected, &args);
    };
    let _ = fs::remove_dir_all(&root);
}
