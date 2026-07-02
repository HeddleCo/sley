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

fn run(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args, None);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_output(program: &str, cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    if let Some(stdin) = stdin {
        sley_testkit::write_stdin_tolerating_early_exit(
            child.stdin.as_mut().expect("stdin pipe"),
            stdin,
        );
    }
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn sley(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_output(sley_testkit::sley_bin!(), cwd, args, stdin)
}

fn git(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    run_output(sley_testkit::oracle_git(), cwd, args, stdin)
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

fn fixture(root: &Path) {
    run(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    write_fixture_contents(root);
}

fn sha256_fixture(root: &Path) {
    run(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    write_fixture_contents(root);
}

fn write_fixture_contents(root: &Path) {
    fs::write(root.join(".gitignore"), b"*.log\n!important.log\ndir/\n").expect("write gitignore");
    fs::write(root.join(".git/info/exclude"), b"*.cache\n").expect("write info exclude");
    fs::write(root.join("global-excludes"), b"*.global\n").expect("write global excludes");
    run(
        sley_testkit::oracle_git(),
        root,
        &["config", "core.excludesFile", "global-excludes"],
    );
    fs::write(root.join("ignored.log"), b"ignored\n").expect("write ignored file");
    fs::write(root.join("important.log"), b"visible\n").expect("write negated file");
    fs::write(root.join("ignored.cache"), b"ignored\n").expect("write info ignored file");
    fs::write(root.join("ignored.global"), b"ignored\n").expect("write global ignored file");
    fs::write(root.join("tracked.log"), b"tracked\n").expect("write tracked ignored file");
    fs::write(root.join("visible.txt"), b"visible\n").expect("write visible file");
    fs::create_dir_all(root.join("dir")).expect("create ignored dir");
    fs::write(root.join("dir/a.txt"), b"ignored\n").expect("write ignored dir file");
    run(
        sley_testkit::oracle_git(),
        root,
        &["add", "-f", "tracked.log"],
    );
}

#[test]
fn check_ignore_matches_upstream_git() {
    let root = unique_temp_dir("check-ignore");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        fixture(&upstream);
        fixture(&rust);

        for (args, stdin) in [
            (
                vec![
                    "check-ignore",
                    "ignored.log",
                    "important.log",
                    "dir",
                    "dir/a.txt",
                    "ignored.cache",
                    "ignored.global",
                    "missing.log",
                    "tracked.log",
                    "visible.txt",
                ],
                None,
            ),
            (
                vec!["check-ignore", "--no-index", "tracked.log", "visible.txt"],
                None,
            ),
            (
                vec![
                    "check-ignore",
                    "-vn",
                    "ignored.log",
                    "important.log",
                    "visible.txt",
                    "missing.log",
                ],
                None,
            ),
            (
                vec![
                    "check-ignore",
                    "-nv",
                    "ignored.log",
                    "important.log",
                    "visible.txt",
                ],
                None,
            ),
            (vec!["check-ignore", "-qv", "ignored.log"], None),
            (vec!["check-ignore", "-vq", "ignored.log"], None),
            (vec!["check-ignore", "-qn", "ignored.log"], None),
            (vec!["check-ignore", "-vz", "ignored.log"], None),
            (
                vec!["check-ignore", "-q", "--no-quiet", "ignored.log"],
                None,
            ),
            (
                vec!["check-ignore", "-v", "--no-verbose", "ignored.log"],
                None,
            ),
            (
                vec![
                    "check-ignore",
                    "-n",
                    "--no-non-matching",
                    "-v",
                    "visible.txt",
                ],
                None,
            ),
            (
                vec!["check-ignore", "--stdin", "--no-stdin", "ignored.log"],
                Some(&b"ignored.cache\n"[..]),
            ),
            (
                vec![
                    "check-ignore",
                    "--no-index",
                    "--index",
                    "tracked.log",
                    "ignored.log",
                ],
                None,
            ),
            (vec!["check-ignore", "-n", "ignored.log"], None),
            (vec!["check-ignore", "-v", "-n", "visible.txt"], None),
            (
                vec![
                    "check-ignore",
                    "-v",
                    "ignored.log",
                    "important.log",
                    "dir",
                    "dir/a.txt",
                    "ignored.cache",
                    "ignored.global",
                    "missing.log",
                    "tracked.log",
                    "visible.txt",
                ],
                None,
            ),
            (vec!["check-ignore", "-q", "ignored.log"], None),
            (vec!["check-ignore", "-q", "visible.txt"], None),
            (
                vec!["check-ignore", "--stdin"],
                Some(&b"ignored.log\nimportant.log\ndir/a.txt\ntracked.log\nvisible.txt\n"[..]),
            ),
            (
                vec!["check-ignore", "--stdin", "--no-index"],
                Some(&b"tracked.log\nvisible.txt\n"[..]),
            ),
            (
                vec!["check-ignore", "--stdin", "-z"],
                Some(&b"ignored.log\0important.log\0dir/a.txt\0visible.txt\0"[..]),
            ),
            (
                vec!["check-ignore", "--stdin", "-z", "-v"],
                Some(&b"ignored.log\0important.log\0dir/a.txt\0visible.txt\0"[..]),
            ),
            (
                vec!["check-ignore", "--stdin", "-z", "-v", "-n"],
                Some(&b"ignored.log\0important.log\0visible.txt\0"[..]),
            ),
            (
                vec!["check-ignore", "-z", "ignored.log", "visible.txt"],
                None,
            ),
        ] {
            let expected = git(&upstream, &args, stdin);
            let actual = sley(&rust, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn check_ignore_sha256_tracked_paths_match_upstream_git() {
    let root = unique_temp_dir("check-ignore-sha256");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        sha256_fixture(&upstream);
        sha256_fixture(&rust);

        for (args, stdin) in [
            (
                vec![
                    "check-ignore",
                    "ignored.log",
                    "important.log",
                    "tracked.log",
                    "visible.txt",
                ],
                None,
            ),
            (
                vec!["check-ignore", "--no-index", "tracked.log", "visible.txt"],
                None,
            ),
            (
                vec!["check-ignore", "--stdin"],
                Some(&b"ignored.log\ntracked.log\nvisible.txt\n"[..]),
            ),
        ] {
            let expected = git(&upstream, &args, stdin);
            let actual = sley(&rust, &args, stdin);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
