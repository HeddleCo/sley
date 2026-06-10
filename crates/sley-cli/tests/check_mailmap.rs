use std::io::Write;
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

fn init_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    std::fs::create_dir_all(&root).expect("create temp dir");
    let status = Command::new(sley_testkit::oracle_git())
        .arg("init")
        .arg(&root)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");
    root
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_input(program: &str, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), cwd, args);
    let actual = run_output(env!("CARGO_BIN_EXE_sley"), cwd, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

fn assert_stdin_match(cwd: &Path, args: &[&str], stdin: &str) {
    let expected = run_output_with_input(sley_testkit::oracle_git(), cwd, args, stdin);
    let actual = run_output_with_input(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

#[test]
fn check_mailmap_matches_upstream_git() {
    let repo = init_repo("check-mailmap");
    std::fs::write(
        repo.join(".mailmap"),
        "\
Proper Name <proper@example.com> <old@example.com>
Canonical <canon@example.com> Alias <alias@example.com>
Email Canon <emailcanon@example.com> <emailonly@example.com>
Only Name <only@example.com> Old Name <only@example.com>
# comment

",
    )
    .expect("write .mailmap");
    std::fs::write(
        repo.join("forms.map"),
        "\
<justcanon@example.com> <justold@example.com>
Name Canon <namecanon@example.com> <nameold@example.com>
Name Only Canon <nameonly@example.com> Old Exact <nameonly@example.com>
Later <later@example.com> <dupe@example.com>
Earlier <earlier@example.com> <dupe@example.com>
",
    )
    .expect("write forms map");
    let blob = run_output(sley_testkit::oracle_git(), &repo, &["hash-object", "-w", "forms.map"]);
    assert!(blob.status.success(), "git hash-object failed");
    let blob = String::from_utf8(blob.stdout).expect("utf8 oid");
    let blob = blob.trim();

    {
        let blob_arg = format!("--mailmap-blob={blob}");
        for args in [
            vec!["check-mailmap", "Someone <old@example.com>"],
            vec!["check-mailmap", "Alias <alias@example.com>"],
            vec!["check-mailmap", "<emailonly@example.com>"],
            vec!["check-mailmap", "Old Name <only@example.com>"],
            vec!["check-mailmap", "No Match <none@example.com>"],
            vec!["check-mailmap", "not-a-contact"],
            vec!["check-mailmap", "Bad <missing"],
            vec!["check-mailmap", "Name <old@example.com> extra"],
            vec![
                "check-mailmap",
                "--mailmap-file=forms.map",
                "Orig Name <justold@example.com>",
            ],
            vec![
                "check-mailmap",
                "--mailmap-file",
                "forms.map",
                "Orig Name <nameold@example.com>",
            ],
            vec![
                "check-mailmap",
                "--mailmap-file=forms.map",
                "Old Exact <nameonly@example.com>",
            ],
            vec![
                "check-mailmap",
                "--mailmap-file=forms.map",
                "Other <nameonly@example.com>",
            ],
            vec![
                "check-mailmap",
                "--mailmap-file=forms.map",
                "X <dupe@example.com>",
            ],
            vec![
                "check-mailmap",
                blob_arg.as_str(),
                "Orig Name <nameold@example.com>",
            ],
            vec!["check-mailmap"],
            vec!["check-mailmap", "--mailmap-file"],
            vec!["check-mailmap", "--mailmap-blob"],
            vec!["check-mailmap", "--unknown", "Someone <old@example.com>"],
        ] {
            assert_status_stdout_stderr_match(&repo, &args);
        }
    }
    assert_stdin_match(
        &repo,
        &["check-mailmap", "--stdin"],
        "\nSomeone <old@example.com>\nAlias <alias@example.com>\n",
    );
    assert_stdin_match(
        &repo,
        &["check-mailmap", "--stdin", "Someone <old@example.com>"],
        "Alias <alias@example.com>\n",
    );

    let _ = std::fs::remove_dir_all(&repo);
}
