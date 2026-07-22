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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_with_identity(cwd: &Path, args: &[&str]) -> Vec<u8> {
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

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

fn prepare_repo(root: &Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("hello.txt"), b"base\n").expect("write base file");
    git(root, &["add", "hello.txt"]);
    run_with_identity(root, &["commit", "-m", "base", "-q"]);
    let base_oid = String::from_utf8(git(root, &["rev-parse", "HEAD"]))
        .expect("base oid utf8")
        .trim()
        .to_string();
    fs::write(root.join("hello.txt"), b"main\n").expect("write main file");
    git(root, &["add", "hello.txt"]);
    run_with_identity(root, &["commit", "-m", "main", "-q"]);
    base_oid
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

fn assert_same_state(upstream: &Path, rust: &Path, expected_file: &[u8]) {
    assert_eq!(
        git(rust, &["branch", "--show-current"]),
        git(upstream, &["branch", "--show-current"]),
        "current branch differed"
    );
    assert_eq!(
        git(rust, &["rev-parse", "HEAD"]),
        git(upstream, &["rev-parse", "HEAD"]),
        "HEAD differed"
    );
    assert_eq!(
        fs::read(rust.join("hello.txt")).expect("read rust file"),
        expected_file,
        "worktree file differed"
    );
    assert_eq!(
        git(rust, &["status", "--short"]),
        git(upstream, &["status", "--short"]),
        "status differed"
    );
}

#[test]
fn checkout_existing_branch_recovers_from_null_direct_head() {
    let root = unique_temp_dir("checkout-null-direct-head");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    prepare_repo(&upstream);
    prepare_repo(&rust);
    let zero = format!("{}\n", "0".repeat(40));
    fs::write(upstream.join(".git/HEAD"), &zero).expect("invalidate upstream HEAD");
    fs::write(rust.join(".git/HEAD"), &zero).expect("invalidate sley HEAD");

    let args = ["checkout", "main", "--"];
    let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
    let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
    assert_same_output(actual, expected, &args);
    assert_same_state(&upstream, &rust, b"main\n");

    let _ = fs::remove_dir_all(&root);
}

/// t2024 #13: `git -c checkout.defaultRemote=… checkout <branch>` must not
/// persist `checkout.defaultRemote` into `.git/config`. A leaked value would
/// make a later multi-remote DWIM checkout pick one remote silently instead of
/// failing with the ambiguity message.
#[test]
fn checkout_dash_c_default_remote_does_not_persist_into_repo_config() {
    // Temp dir name must not contain the substring "defaultremote" — remote
    // URLs embed the absolute path and would false-positive the assertion.
    let root = unique_temp_dir("checkout-c-no-persist");
    let repo = root.join("repo");
    let remote_a = root.join("remote_a");
    let remote_b = root.join("remote_b");
    fs::create_dir_all(&repo).expect("create repo");
    fs::create_dir_all(&remote_a).expect("create remote_a");
    fs::create_dir_all(&remote_b).expect("create remote_b");

    prepare_repo(&repo);
    for (remote, body) in [(&remote_a, b"from-a\n" as &[u8]), (&remote_b, b"from-b\n")] {
        git(remote, &["init", "-q", "-b", "main"]);
        fs::write(remote.join("hello.txt"), b"base\n").expect("write base file");
        git(remote, &["add", "hello.txt"]);
        run_with_identity(remote, &["commit", "-m", "base", "-q"]);
        git(remote, &["checkout", "-q", "-b", "shared"]);
        fs::write(remote.join("hello.txt"), body).expect("write shared branch file");
        git(remote, &["add", "hello.txt"]);
        run_with_identity(remote, &["commit", "-m", "shared", "-q"]);
    }

    git(&repo, &["remote", "add", "repo_a", remote_a.to_str().unwrap()]);
    git(&repo, &["remote", "add", "repo_b", remote_b.to_str().unwrap()]);
    git(&repo, &["fetch", "--all", "-q"]);

    // First DWIM with an explicit defaultRemote via `-c` should succeed and set
    // tracking — but must leave no checkout.defaultRemote on disk.
    let out = run_output(
        sley_testkit::sley_bin!(),
        &repo,
        &[
            "-c",
            "checkout.defaultRemote=repo_a",
            "checkout",
            "shared",
        ],
    );
    assert!(
        out.status.success(),
        "sley -c checkout.defaultRemote checkout shared failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let config = fs::read_to_string(repo.join(".git/config")).expect("read config");
    assert!(
        !config
            .lines()
            .any(|line| line.to_ascii_lowercase().contains("defaultremote")),
        "command-line checkout.defaultRemote was persisted into .git/config:\n{config}"
    );

    // Drop the local branch so the next DWIM is ambiguous across remotes again.
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["branch", "-D", "shared"]);

    let ambiguous = run_output(sley_testkit::sley_bin!(), &repo, &["checkout", "shared"]);
    assert_eq!(
        ambiguous.status.code(),
        Some(128),
        "ambiguous multi-remote DWIM should exit 128, got {:?}\nstdout:\n{}\nstderr:\n{}",
        ambiguous.status.code(),
        String::from_utf8_lossy(&ambiguous.stdout),
        String::from_utf8_lossy(&ambiguous.stderr)
    );
    let stderr = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(
        stderr.contains("matched multiple") && stderr.contains("remote tracking branches"),
        "expected multi-remote ambiguity message, got:\n{stderr}"
    );
    assert!(
        !repo.join(".git/refs/heads/shared").exists(),
        "ambiguous DWIM must not create the local branch"
    );

    let _ = fs::remove_dir_all(&root);
}

/// t2024 #20: when `branch.<name>.remote=.` and `branch.<name>.merge` is a short
/// name (`main`) rather than `refs/heads/main`, checkout must report tracking
/// status identically to the fully-qualified form ("behind … by N commits").
#[test]
fn checkout_loosely_defined_local_base_branch_reported_like_strict() {
    let root = unique_temp_dir("checkout-loose-local-base");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream");
    fs::create_dir_all(&rust).expect("create rust");
    for repo in [&upstream, &rust] {
        prepare_repo(repo);
        git(repo, &["branch", "strict"]);
        git(repo, &["branch", "loose"]);
        run_with_identity(repo, &["commit", "--allow-empty", "-m", "a bit more", "-q"]);
        git(repo, &["config", "branch.strict.remote", "."]);
        git(repo, &["config", "branch.loose.remote", "."]);
        git(repo, &["config", "branch.strict.merge", "refs/heads/main"]);
        git(repo, &["config", "branch.loose.merge", "main"]);
    }

    fn combined(out: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    let git_strict = run_output(sley_testkit::oracle_git(), &upstream, &["checkout", "strict"]);
    let git_loose = {
        git(&upstream, &["checkout", "-q", "main"]);
        run_output(sley_testkit::oracle_git(), &upstream, &["checkout", "loose"])
    };
    assert!(git_strict.status.success() && git_loose.status.success());
    let git_expect = combined(&git_strict).replace("strict", "BRANCHNAME");
    let git_actual = combined(&git_loose).replace("loose", "BRANCHNAME");
    assert_eq!(
        git_expect, git_actual,
        "oracle git strict vs loose tracking report differed"
    );

    let sley_strict = run_output(sley_testkit::sley_bin!(), &rust, &["checkout", "strict"]);
    let sley_loose = {
        git(&rust, &["checkout", "-q", "main"]);
        run_output(sley_testkit::sley_bin!(), &rust, &["checkout", "loose"])
    };
    assert_same_output(sley_strict, git_strict, &["checkout", "strict"]);
    assert_same_output(sley_loose, git_loose, &["checkout", "loose"]);

    // And sley's own strict/loose reports must match after name normalization.
    let sley_strict2 = {
        git(&rust, &["checkout", "-q", "main"]);
        run_output(sley_testkit::sley_bin!(), &rust, &["checkout", "strict"])
    };
    let sley_loose2 = {
        git(&rust, &["checkout", "-q", "main"]);
        run_output(sley_testkit::sley_bin!(), &rust, &["checkout", "loose"])
    };
    let sley_expect = combined(&sley_strict2).replace("strict", "BRANCHNAME");
    let sley_actual = combined(&sley_loose2).replace("loose", "BRANCHNAME");
    assert_eq!(
        sley_expect, sley_actual,
        "sley strict vs loose tracking report differed:\nexpect:\n{sley_expect}\nactual:\n{sley_actual}"
    );
    assert!(
        sley_actual.contains("behind") && sley_actual.contains("BRANCHNAME"),
        "expected behind-tracking report, got:\n{sley_actual}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn checkout_branch_creation_and_quiet_match_upstream_git() {
    let root = unique_temp_dir("checkout-branch-create");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        let base_oid = prepare_repo(&upstream);
        prepare_repo(&rust);

        let args = ["checkout", "-b", "topic", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["checkout", "-q", "--no-quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-q", "-b", "side"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-B", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = [
            "checkout",
            "--no-progress",
            "--no-guess",
            "--ignore-other-worktrees",
            "--no-ignore-other-worktrees",
            "--no-recurse-submodules",
            "main",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["checkout", "-B", "fresh", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["checkout", "-q", "-B", "quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn switch_branch_creation_and_force_create_match_upstream_git() {
    let root = unique_temp_dir("switch-branch-create");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        let base_oid = prepare_repo(&upstream);
        prepare_repo(&rust);

        git(&upstream, &["branch", "topic", base_oid.as_str()]);
        git(&rust, &["branch", "topic", base_oid.as_str()]);

        let args = ["switch", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = ["switch", "-q", "--no-quiet", "main"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-c", "side", base_oid.as_str()];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"base\n");

        let args = [
            "switch",
            "--no-progress",
            "--no-guess",
            "--ignore-other-worktrees",
            "--no-ignore-other-worktrees",
            "--no-recurse-submodules",
            "main",
        ];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-C", "topic"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");

        let args = ["switch", "-q", "--create=quiet"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_same_state(&upstream, &rust, b"main\n");
    };
    let _ = fs::remove_dir_all(&root);
}
