use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
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
        input,
    );
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn init_repo(program: &str, root: &Path) {
    fs::create_dir_all(root).expect("create repository directory");
    let output = run(program, root, &["init", "-q", "-b", "main"]);
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn empty_commit_stream(count: usize) -> Vec<u8> {
    let mut stream = String::new();
    for i in 1..=count {
        writeln!(stream, "commit refs/heads/main").expect("write stream");
        writeln!(
            stream,
            "committer A U Thor <author@example.com> {} +0200",
            1_000_000_000_u64 + (i as u64 * 100)
        )
        .expect("write stream");
        writeln!(stream, "data <<EOF\ncommit #{i}\nEOF").expect("write stream");
    }
    stream.into_bytes()
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn eight_thousand_commit_import_matches_oracle_and_uses_one_pack() {
    let root = unique_temp_dir("fast-import-8k");
    let oracle_root = root.join("oracle");
    let sley_root = root.join("sley");
    init_repo(sley_testkit::oracle_git(), &oracle_root);
    init_repo(sley_testkit::sley_bin!(), &sley_root);
    let stream = empty_commit_stream(8_000);

    let oracle_start = Instant::now();
    let oracle = run_with_stdin(
        sley_testkit::oracle_git(),
        &oracle_root,
        &["fast-import", "--quiet"],
        &stream,
    );
    let oracle_elapsed = oracle_start.elapsed();
    assert_success(&oracle, "oracle fast-import");

    let sley_start = Instant::now();
    let sley = run_with_stdin(
        sley_testkit::sley_bin!(),
        &sley_root,
        &["fast-import", "--quiet"],
        &stream,
    );
    let sley_elapsed = sley_start.elapsed();
    assert_success(&sley, "sley fast-import");

    let oracle_tip = run(
        sley_testkit::oracle_git(),
        &oracle_root,
        &["rev-parse", "main"],
    );
    let sley_tip = run(
        sley_testkit::oracle_git(),
        &sley_root,
        &["rev-parse", "main"],
    );
    assert_success(&oracle_tip, "read oracle tip");
    assert_success(&sley_tip, "read sley tip");
    assert_eq!(sley_tip.stdout, oracle_tip.stdout, "imported tips differ");

    let count = run(
        sley_testkit::oracle_git(),
        &sley_root,
        &["rev-list", "--count", "main"],
    );
    assert_success(&count, "count imported commits");
    assert_eq!(count.stdout, b"8000\n");

    let pack_dir = sley_root.join(".git/objects/pack");
    let pack_files = fs::read_dir(&pack_dir)
        .expect("read pack directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "pack"))
        .count();
    assert_eq!(pack_files, 1, "large import should install one pack");

    eprintln!("8k equal-work fast-import: sley={sley_elapsed:?} oracle={oracle_elapsed:?}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_stream_before_checkpoint_does_not_publish_a_partial_branch() {
    let root = unique_temp_dir("fast-import-no-partial-ref");
    init_repo(sley_testkit::sley_bin!(), &root);
    let mut stream = empty_commit_stream(1);
    stream.extend_from_slice(
        b"tag partial-tag\n\
          from refs/heads/main\n\
          tagger A U Thor <author@example.com> 1000000200 +0200\n\
          data <<EOF\n\
          partial tag\n\
          EOF\n\
          unsupported-command\n",
    );

    let output = run_with_stdin(
        sley_testkit::sley_bin!(),
        &root,
        &["fast-import", "--quiet"],
        &stream,
    );
    assert!(!output.status.success(), "malformed stream should fail");
    let branch = run(
        sley_testkit::oracle_git(),
        &root,
        &["show-ref", "--verify", "refs/heads/main"],
    );
    assert!(
        !branch.status.success(),
        "pre-checkpoint branch update leaked from failed import"
    );
    let tag = run(
        sley_testkit::oracle_git(),
        &root,
        &["show-ref", "--verify", "refs/tags/partial-tag"],
    );
    assert!(
        !tag.status.success(),
        "pre-checkpoint tag update leaked from failed import"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoint_publishes_only_the_completed_prefix_before_a_later_error() {
    let root = unique_temp_dir("fast-import-checkpoint-prefix");
    init_repo(sley_testkit::sley_bin!(), &root);
    let mut stream = empty_commit_stream(1);
    stream.extend_from_slice(b"checkpoint\nunsupported-command\n");

    let output = run_with_stdin(
        sley_testkit::sley_bin!(),
        &root,
        &["fast-import", "--quiet"],
        &stream,
    );
    assert!(!output.status.success(), "malformed suffix should fail");
    let count = run(
        sley_testkit::oracle_git(),
        &root,
        &["rev-list", "--count", "main"],
    );
    assert_success(&count, "read checkpointed branch");
    assert_eq!(count.stdout, b"1\n");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn required_done_missing_does_not_publish_staged_objects_or_refs() {
    let root = unique_temp_dir("fast-import-require-done");
    init_repo(sley_testkit::sley_bin!(), &root);
    let stream = empty_commit_stream(1);

    let output = run_with_stdin(
        sley_testkit::sley_bin!(),
        &root,
        &["fast-import", "--quiet", "--done"],
        &stream,
    );
    assert!(!output.status.success(), "missing done should fail");
    let branch = run(
        sley_testkit::oracle_git(),
        &root,
        &["show-ref", "--verify", "refs/heads/main"],
    );
    assert!(!branch.status.success(), "missing done published a branch");
    let loose_objects = fs::read_dir(root.join(".git/objects"))
        .expect("read object directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_name().to_string_lossy().len() == 2
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .count();
    assert_eq!(loose_objects, 0, "staged objects escaped failed import");
    let _ = fs::remove_dir_all(root);
}
