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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_identity(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Example User")
        .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "Example User")
        .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
        .env("GIT_COMMITTER_DATE", "@0 +0000")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(cwd: &Path, args: &[&str]) {
    run_success(sley_testkit::oracle_git(), cwd, args);
}

fn git_with_identity(cwd: &Path, args: &[&str]) {
    let output = run_output_with_identity(sley_testkit::oracle_git(), cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepare_identity(root: &Path) {
    git(root, &["config", "user.name", "Example User"]);
    git(root, &["config", "user.email", "example@example.invalid"]);
}

fn prepare_pull_clone(upstream: &Path, clone: &Path) {
    git(clone, &["init", "-q", "-b", "master"]);
    prepare_identity(clone);
    fs::write(clone.join("hello.txt"), b"base\n").expect("write base file");
    git(clone, &["add", "hello.txt"]);
    git_with_identity(clone, &["commit", "-m", "base", "-q"]);
    let upstream_arg = upstream.to_str().expect("upstream path is utf8");
    git(clone, &["remote", "add", "origin", upstream_arg]);
    git(clone, &["fetch", "origin", "-q"]);
    git(
        clone,
        &["branch", "--set-upstream-to=origin/master", "master"],
    );
    git(clone, &["config", "pull.rebase", "false"]);
}

fn prepare_fast_forward_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("hello.txt"), b"base\n").expect("write base file");
    git(upstream, &["add", "hello.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    let base = String::from_utf8(
        run_output(sley_testkit::oracle_git(), upstream, &["rev-parse", "HEAD"]).stdout,
    )
    .expect("base oid utf8")
    .trim()
    .to_string();
    git(upstream, &["checkout", "-b", "topic", &base, "-q"]);
    fs::write(upstream.join("topic.txt"), b"topic\n").expect("write topic file");
    git(upstream, &["add", "topic.txt"]);
    git_with_identity(upstream, &["commit", "-m", "topic", "-q"]);
    git(upstream, &["checkout", "master", "-q"]);
    git(upstream, &["merge", "topic", "-q"]);
}

fn prepare_fast_forward_clone(upstream: &Path, clone: &Path) {
    prepare_pull_clone(upstream, clone);
}

fn prepare_three_way_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("shared.txt"), b"base\n").expect("write shared file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    fs::write(upstream.join("shared.txt"), b"upstream\n").expect("write upstream file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "upstream", "-q"]);
}

fn prepare_three_way_clone(upstream: &Path, clone: &Path) {
    git(clone, &["init", "-q", "-b", "master"]);
    prepare_identity(clone);
    fs::write(clone.join("shared.txt"), b"base\n").expect("write shared file");
    git(clone, &["add", "shared.txt"]);
    git_with_identity(clone, &["commit", "-m", "base", "-q"]);
    prepare_pull_clone(upstream, clone);
    fs::write(clone.join("local.txt"), b"local\n").expect("write local file");
    git(clone, &["add", "local.txt"]);
    git_with_identity(clone, &["commit", "-m", "local", "-q"]);
}

#[test]
fn pull_fast_forward_matches_upstream_git() {
    let root = unique_temp_dir("pull-fast-forward");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_fast_forward_upstream(&upstream);
    prepare_fast_forward_clone(&upstream, &expected);
    prepare_fast_forward_clone(&upstream, &actual);
    let args = ["pull"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull fast-forward"
    );
    assert!(
        actual_output.status.success(),
        "sley pull failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    let actual_stdout = String::from_utf8_lossy(&actual_output.stdout);
    assert!(
        actual_stdout.contains("Fast-forward"),
        "expected Fast-forward in output"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after fast-forward pull"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pull_three_way_clean_matches_upstream_git() {
    let root = unique_temp_dir("pull-three-way-clean");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_three_way_upstream(&upstream);
    prepare_three_way_clone(&upstream, &expected);
    prepare_three_way_clone(&upstream, &actual);
    let args = ["pull"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull three-way"
    );
    assert!(
        actual_output.status.success(),
        "sley pull failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&actual_output.stdout).contains("ort"),
        "expected ort merge summary in output"
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after three-way pull"
    );
    let _ = fs::remove_dir_all(&root);
}

/// t5521-pull-options: non-quiet `pull --no-rebase` / `--rebase` / `-v` into an
/// unborn branch must print fetch status on stderr (the `From …` / `* branch …`
/// → `FETCH_HEAD` lines) and keep stdout empty. Quiet and last-flag-wins
/// `-v -q` must silence stderr.
#[test]
fn pull_options_verbosity_stderr_matches_t5521() {
    let root = unique_temp_dir("pull-options-verbosity");
    let parent = root.join("parent");
    fs::create_dir_all(&parent).expect("create parent");
    git(&parent, &["init", "-q", "-b", "main"]);
    prepare_identity(&parent);
    fs::write(parent.join("file"), b"one\n").expect("write file");
    git(&parent, &["add", "file"]);
    git_with_identity(&parent, &["commit", "-m", "one", "-q"]);
    let parent_arg = parent.to_str().expect("parent path utf8");

    let cases: &[(&str, &[&str], bool)] = &[
        ("no-rebase", &["pull", "--no-rebase", parent_arg], true),
        ("rebase", &["pull", "--rebase", parent_arg], true),
        (
            "v-no-rebase",
            &["pull", "-v", "--no-rebase", parent_arg],
            true,
        ),
        ("v-rebase", &["pull", "-v", "--rebase", parent_arg], true),
        (
            "q-no-rebase",
            &["pull", "-q", "--no-rebase", parent_arg],
            false,
        ),
        (
            "q-v-no-rebase",
            &["pull", "-q", "-v", "--no-rebase", parent_arg],
            true,
        ),
        (
            "v-q-no-rebase",
            &["pull", "-v", "-q", "--no-rebase", parent_arg],
            false,
        ),
    ];
    for (label, args, expect_stderr) in cases {
        let cloned = root.join(format!("cloned-{label}"));
        fs::create_dir_all(&cloned).expect("create clone dir");
        git(&cloned, &["init", "-q", "-b", "main"]);
        prepare_identity(&cloned);
        let output = run_output_with_identity(sley_testkit::sley_bin!(), &cloned, args);
        assert!(
            output.status.success(),
            "pull {label} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "pull {label}: expected empty stdout, got:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        if *expect_stderr {
            assert!(
                !output.stderr.is_empty(),
                "pull {label}: expected non-empty stderr (fetch status)"
            );
            let err = String::from_utf8_lossy(&output.stderr);
            assert!(
                err.contains("From ") || err.contains("FETCH_HEAD"),
                "pull {label}: stderr should look like fetch status, got:\n{err}"
            );
        } else {
            assert!(
                output.stderr.is_empty(),
                "pull {label}: expected empty stderr under quiet, got:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let _ = fs::remove_dir_all(&root);
}

/// t5521-pull-options #12: `pull --no-rebase --all --force` must force a
/// non-fast-forward update of a local tracking branch (fetch-side `--force`,
/// not `--force-rebase`).
#[test]
fn pull_force_allows_non_fast_forward_tracking_update() {
    let root = unique_temp_dir("pull-options-force");
    let parent = root.join("parent");
    let cloned = root.join("cloned");
    fs::create_dir_all(&parent).expect("create parent");
    fs::create_dir_all(&cloned).expect("create clone");
    git(&parent, &["init", "-q", "-b", "main"]);
    prepare_identity(&parent);
    fs::write(parent.join("file"), b"one\n").expect("write file");
    git(&parent, &["add", "file"]);
    git_with_identity(&parent, &["commit", "-m", "one", "-q"]);

    git(&cloned, &["init", "-q", "-b", "main"]);
    prepare_identity(&cloned);
    let parent_arg = parent.to_str().expect("parent path utf8");
    // Append remotes (do not clobber user.identity written by prepare_identity).
    let config = format!(
        r#"
[remote "one"]
	url = {parent_arg}
	fetch = refs/heads/main:refs/heads/mirror
[remote "two"]
	url = {parent_arg}
	fetch = refs/heads/main:refs/heads/origin
[branch "main"]
	remote = two
	merge = refs/heads/main
"#
    );
    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(cloned.join(".git/config"))
            .expect("open config");
        f.write_all(config.as_bytes()).expect("append config");
    }
    // Seed via remote "two" so origin tracks parent's main.
    let seed = run_output_with_identity(sley_testkit::sley_bin!(), &cloned, &["pull", "two"]);
    assert!(
        seed.status.success(),
        "seed pull two failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    fs::write(cloned.join("A"), b"A\n").expect("write A");
    git(&cloned, &["add", "A"]);
    git_with_identity(&cloned, &["commit", "-m", "A", "-q"]);
    git(&cloned, &["branch", "-f", "origin"]);

    // Without --force this must reject (non-fast-forward origin).
    let rejected = run_output_with_identity(
        sley_testkit::sley_bin!(),
        &cloned,
        &["pull", "--no-rebase", "--all"],
    );
    assert!(
        !rejected.status.success(),
        "expected non-force pull to reject non-ff tracking update"
    );
    let reject_err = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        reject_err.contains("rejected") || reject_err.contains("non-fast-forward"),
        "expected non-ff rejection, got:\n{reject_err}"
    );

    let forced = run_output_with_identity(
        sley_testkit::sley_bin!(),
        &cloned,
        &["pull", "--no-rebase", "--all", "--force"],
    );
    assert!(
        forced.status.success(),
        "pull --force should allow non-ff tracking update: status={:?}\nstdout:\n{}\nstderr:\n{}",
        forced.status.code(),
        String::from_utf8_lossy(&forced.stdout),
        String::from_utf8_lossy(&forced.stderr)
    );
    // origin must now match parent's main (forced back), not the local commit A.
    let origin = run_output(sley_testkit::sley_bin!(), &cloned, &["rev-parse", "origin"]);
    let parent_main = run_output(sley_testkit::sley_bin!(), &parent, &["rev-parse", "main"]);
    assert_eq!(
        origin.stdout, parent_main.stdout,
        "origin should be forced back to parent main"
    );
    let _ = fs::remove_dir_all(&root);
}
