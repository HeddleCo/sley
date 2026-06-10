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

fn run_success_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(sley_testkit::oracle_git())
        .current_dir(cwd)
        .env("GIT_AUTHOR_DATE", "1970-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "1970-01-01T00:00:00 +0000")
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

fn prepare_pack_refs_repo(root: &Path) {
    fs::create_dir_all(root).expect("create repo");
    run_success(sley_testkit::oracle_git(), root, &["init", "-q"]);
    run_success_with_identity(root, &["commit", "--allow-empty", "-qm", "initial"]);
    run_success(sley_testkit::oracle_git(), root, &["branch", "topic"]);
    run_success(sley_testkit::oracle_git(), root, &["tag", "light"]);
    run_success_with_identity(root, &["tag", "-a", "ann", "-m", "annotated"]);
}

fn read_packed_refs(root: &Path) -> Vec<u8> {
    fs::read(root.join(".git/packed-refs")).unwrap_or_default()
}

fn loose_ref_exists(root: &Path, name: &str) -> bool {
    root.join(".git").join(name).is_file()
}

#[test]
fn pack_refs_modes_match_upstream_git() {
    let root = unique_temp_dir("pack-refs");
    for args in [
        vec!["pack-refs"],
        vec!["pack-refs", "--all", "--prune"],
        vec!["pack-refs", "--all", "--no-prune"],
        vec!["pack-refs", "--no-all", "--prune"],
        vec!["pack-refs", "--include", "refs/heads/topic"],
        vec![
            "pack-refs",
            "--include=refs/heads/*",
            "--exclude",
            "refs/heads/main",
        ],
        vec!["pack-refs", "--include", "refs/heads/*", "--no-include"],
        vec![
            "pack-refs",
            "--all",
            "--exclude=refs/heads/topic",
            "--no-exclude",
        ],
        vec!["pack-refs", "--bogus"],
        vec!["pack-refs", "--no-include=refs/heads/*"],
    ] {
        let upstream = root.join(format!("upstream-{}", args.join("-").replace('/', "_")));
        let actual = root.join(format!("actual-{}", args.join("-").replace('/', "_")));
        prepare_pack_refs_repo(&upstream);
        prepare_pack_refs_repo(&actual);

        let expected = run(sley_testkit::oracle_git(), &upstream, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected, &args);
        assert_eq!(
            read_packed_refs(&actual),
            read_packed_refs(&upstream),
            "packed-refs differed for {args:?}"
        );
        for name in [
            "refs/heads/main",
            "refs/heads/topic",
            "refs/tags/light",
            "refs/tags/ann",
        ] {
            assert_eq!(
                loose_ref_exists(&actual, name),
                loose_ref_exists(&upstream, name),
                "loose ref {name} presence differed for {args:?}"
            );
        }
        assert_eq!(
            run(env!("CARGO_BIN_EXE_sley"), &actual, &["show-ref"]).stdout,
            run(sley_testkit::oracle_git(), &upstream, &["show-ref"]).stdout,
            "show-ref differed after {args:?}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
