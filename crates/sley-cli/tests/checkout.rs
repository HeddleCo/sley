use std::fs;
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

fn run_with_input(program: &str, cwd: &Path, args: &[&str], input: &[u8]) -> Output {
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
        .take()
        .expect("interactive stdin")
        .write_all(input)
        .expect("write interactive input");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
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

    git(
        &repo,
        &[
            "remote",
            "add",
            "repo_a",
            remote_a.to_str().expect("UTF-8 remote A path"),
        ],
    );
    git(
        &repo,
        &[
            "remote",
            "add",
            "repo_b",
            remote_b.to_str().expect("UTF-8 remote B path"),
        ],
    );
    git(&repo, &["fetch", "--all", "-q"]);

    // First DWIM with an explicit defaultRemote via `-c` should succeed and set
    // tracking — but must leave no checkout.defaultRemote on disk.
    let out = run_output(
        sley_testkit::sley_bin!(),
        &repo,
        &["-c", "checkout.defaultRemote=repo_a", "checkout", "shared"],
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

    let git_strict = run_output(
        sley_testkit::oracle_git(),
        &upstream,
        &["checkout", "strict"],
    );
    let git_loose = {
        git(&upstream, &["checkout", "-q", "main"]);
        run_output(
            sley_testkit::oracle_git(),
            &upstream,
            &["checkout", "loose"],
        )
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

/// t2016: `checkout -p HEAD` / `@` with no staged changes — rejecting
/// "Apply them to the worktree anyway?" must leave index and worktree
/// untouched. Post-apply `update-index --refresh -- <path>` must not re-stage
/// the dirty worktree (matrix regression: 19→17).
#[test]
fn checkout_patch_head_no_staged_abort_preserves_state() {
    let root = unique_temp_dir("checkout-patch-head-abort");
    fs::create_dir_all(&root).expect("create root");
    {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/foo"), b"parent\n").expect("write parent");
        fs::write(repo.join("bar"), b"dummy\n").expect("write bar");
        git(&repo, &["add", "bar", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "initial", "-q"]);
        fs::write(repo.join("dir/foo"), b"head\n").expect("write head");
        git(&repo, &["add", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "second", "-q"]);

        for treeish in ["HEAD", "@"] {
            // NO staged: index matches HEAD, worktree dirty
            fs::write(repo.join("dir/foo"), b"head\n").expect("index=head");
            git(&repo, &["add", "dir/foo"]);
            fs::write(repo.join("dir/foo"), b"work\n").expect("worktree=work");
            fs::write(repo.join("bar"), b"bar_index\n").expect("bar index");
            git(&repo, &["add", "bar"]);
            fs::write(repo.join("bar"), b"bar_work\n").expect("bar work");

            let args = ["checkout", "-p", treeish];
            // n=skip bar, y=select dir/foo, n=refuse worktree-only apply
            let output = run_with_input(sley_testkit::sley_bin!(), &repo, &args, b"n\ny\nn\n");
            assert!(
                output.status.success(),
                "checkout -p {treeish} abort failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("Discard"),
                "expected Discard prompt for checkout -p {treeish}, got:\n{stdout}"
            );
            assert_eq!(
                fs::read(repo.join("dir/foo")).expect("read worktree"),
                b"work\n",
                "abort must leave worktree dirty for {treeish}"
            );
            assert_eq!(
                git(&repo, &["show", ":dir/foo"]),
                b"head\n",
                "abort must not re-stage dirty worktree into index for {treeish}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

/// t2016 apply half: accept worktree-only apply when index does not take the hunk.
#[test]
fn checkout_patch_head_no_staged_apply_worktree_only() {
    let root = unique_temp_dir("checkout-patch-head-apply");
    fs::create_dir_all(&root).expect("create root");
    {
        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/foo"), b"parent\n").expect("write parent");
        fs::write(repo.join("bar"), b"dummy\n").expect("write bar");
        git(&repo, &["add", "bar", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "initial", "-q"]);
        fs::write(repo.join("dir/foo"), b"head\n").expect("write head");
        git(&repo, &["add", "dir/foo"]);
        run_with_identity(&repo, &["commit", "-m", "second", "-q"]);

        fs::write(repo.join("dir/foo"), b"head\n").expect("index=head");
        git(&repo, &["add", "dir/foo"]);
        fs::write(repo.join("dir/foo"), b"work\n").expect("worktree=work");
        fs::write(repo.join("bar"), b"bar_index\n").expect("bar index");
        git(&repo, &["add", "bar"]);
        fs::write(repo.join("bar"), b"bar_work\n").expect("bar work");

        let output = run_with_input(
            sley_testkit::sley_bin!(),
            &repo,
            &["checkout", "-p", "HEAD"],
            b"n\ny\ny\n",
        );
        assert!(
            output.status.success(),
            "checkout -p HEAD apply failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Discard"),
            "expected Discard prompt"
        );
        assert_eq!(
            fs::read(repo.join("dir/foo")).expect("read worktree"),
            b"head\n",
            "accepting worktree-only apply restores worktree to HEAD"
        );
        assert_eq!(
            git(&repo, &["show", ":dir/foo"]),
            b"head\n",
            "index already matched HEAD and must stay that way"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// FIX-D regression tests
// ---------------------------------------------------------------------------

/// Shared fixture for the global-config cascade tests: two identical repos
/// (upstream driven by oracle git, rust by sley) plus a temp `$HOME` holding a
/// `~/.gitconfig`. Returns `(root, upstream, rust, home)`.
struct GlobalConfigFixture {
    root: PathBuf,
    upstream: PathBuf,
    rust: PathBuf,
    home: PathBuf,
}

fn prepare_global_config_fixture(name: &str, global_gitconfig: &[u8]) -> GlobalConfigFixture {
    let root = unique_temp_dir(name);
    let home = root.join("home");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&home).expect("create temp home");
    fs::write(home.join(".gitconfig"), global_gitconfig).expect("write global gitconfig");
    for repo in [&upstream, &rust] {
        fs::create_dir_all(repo).expect("create repo dir");
        git(repo, &["init", "-q", "-b", "main"]);
        // The attribute binding is committed: checkout resolves filter
        // attributes from the tree/index, not an untracked worktree file.
        fs::write(repo.join(".gitattributes"), b"hello.txt filter=upper\n")
            .expect("write .gitattributes");
        fs::write(repo.join("hello.txt"), b"base\n").expect("write base file");
        git(repo, &["add", ".gitattributes", "hello.txt"]);
        run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        fs::write(repo.join("hello.txt"), b"main\n").expect("write main file");
        git(repo, &["add", "hello.txt"]);
        run_with_identity(repo, &["commit", "-m", "main", "-q"]);
    }
    GlobalConfigFixture {
        root,
        upstream,
        rust,
        home,
    }
}

/// Run a command with the fixture's temp `$HOME` as the global-config source.
fn run_with_global_home(program: &str, cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_SYSTEM")
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

/// S11 regression: `checkout -B <branch> <start>` must honour smudge filters
/// defined in the *global* config. The branch-reset path previously read the
/// repo-only config, so the same command saw different filters than an ordinary
/// switch.
#[test]
fn checkout_branch_reset_honors_global_smudge_filter() {
    let fx = prepare_global_config_fixture(
        "checkout-b-global-smudge",
        b"[filter \"upper\"]\n\tsmudge = tr a-z A-Z\n\tclean = tr A-Z a-z\n",
    );

    // Base commit blob content is lowercase ("base\n"); the clean filter keeps
    // it lowercase, so both repos hold identical objects.
    let base_oid = String::from_utf8(git(&fx.upstream, &["rev-parse", "HEAD~"]))
        .expect("base oid utf8")
        .trim()
        .to_string();

    let args = ["checkout", "-B", "topic", base_oid.as_str()];
    let expected = run_with_global_home(sley_testkit::oracle_git(), &fx.upstream, &fx.home, &args);
    let actual = run_with_global_home(sley_testkit::sley_bin!(), &fx.rust, &fx.home, &args);
    assert_same_output(actual, expected, &args);

    // The reset re-materialized hello.txt from the base tree through the
    // global smudge filter → uppercase on disk in BOTH repos.
    assert_eq!(
        fs::read(fx.rust.join("hello.txt")).expect("read sley hello.txt"),
        b"BASE\n",
        "sley did not apply the global smudge filter on the -B path"
    );
    assert_eq!(
        fs::read(fx.upstream.join("hello.txt")).expect("read git hello.txt"),
        b"BASE\n",
        "oracle fixture sanity: expected smudged content"
    );

    let _ = fs::remove_dir_all(&fx.root);
}

/// `-c include.path` parity: relative include paths from the command line are
/// rejected exactly like upstream (they must come from files), and an absolute
/// path is honoured during checkout — observable through
/// `advice.detachedHead=false` suppressing the detached-HEAD advice block.
#[test]
fn checkout_dash_c_include_path_matches_oracle_cwd_and_absolute_semantics() {
    let root = unique_temp_dir("checkout-c-include-path");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    for repo in [&upstream, &rust] {
        fs::create_dir_all(repo.join("sub")).expect("create repo dir");
        git(repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("f.txt"), b"one\n").expect("write f");
        git(repo, &["add", "f.txt"]);
        run_with_identity(repo, &["commit", "-m", "first", "-q"]);
        fs::write(repo.join("f.txt"), b"two\n").expect("write f two");
        git(repo, &["add", "f.txt"]);
        run_with_identity(repo, &["commit", "-m", "second", "-q"]);
        // Include lives in the SUBDIRECTORY; running from there proves the
        // command-line include context behaves identically in both binaries.
        fs::write(
            repo.join("sub").join("inc.inc"),
            b"[advice]\n\tdetachedHead = false\n",
        )
        .expect("write inc.inc");
    }

    // (1) Relative include from `-c`: both refuse with the same fatal message.
    // (The surrounding error framing differs cosmetically between binaries;
    // the contract under test is cwd-based resolution + rejection.)
    let rel_args = ["-c", "include.path=inc.inc", "checkout", "--detach", "HEAD"];
    let expected = run_output(sley_testkit::oracle_git(), &upstream.join("sub"), &rel_args);
    assert_eq!(
        expected.status.code(),
        Some(128),
        "oracle must reject relative -c include.path"
    );
    assert!(
        String::from_utf8_lossy(&expected.stderr)
            .contains("relative config includes must come from files"),
        "oracle stderr missing include rejection message"
    );
    let actual = run_output(sley_testkit::sley_bin!(), &rust.join("sub"), &rel_args);
    assert_eq!(actual.status.code(), expected.status.code());
    assert!(
        String::from_utf8_lossy(&actual.stderr)
            .contains("relative config includes must come from files"),
        "sley stderr missing include rejection message"
    );

    // (2) Absolute include from `-c`: advice suppressed in both stderr streams.
    for repo in [&upstream, &rust] {
        let inc = repo.join("sub").join("inc.inc");
        let abs_arg = format!("include.path={}", inc.display());
        let output = Command::new(if repo == &upstream {
            sley_testkit::oracle_git()
        } else {
            sley_testkit::sley_bin!()
        })
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", &abs_arg, "checkout", "--detach", "HEAD"])
        .output()
        .unwrap_or_else(|err| panic!("failed detached checkout: {err}"));
        assert!(output.status.success(), "absolute include checkout failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("detached HEAD"),
            "advice.detachedHead=false via include.path was not honoured:\n{stderr}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// S12 regression: when a D/F transition replaces a tracked directory with a
/// file, ignored files under that directory do not block the update (they are
/// removed with the subtree) while untracked non-ignored files still abort.
#[test]
fn checkout_df_transition_ignored_files_do_not_block_directory_replacement() {
    let root = unique_temp_dir("checkout-df-ignored-subdir");

    let build = |repo: &Path| {
        fs::create_dir_all(repo).expect("create repo dir");
        git(repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/file.txt"), b"tracked\n").expect("write tracked file");
        fs::write(repo.join("keep.txt"), b"x\n").expect("write keep");
        git(repo, &["add", "."]);
        run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        // side replaces directory `dir` with a FILE named `dir`.
        git(repo, &["checkout", "-q", "-b", "side"]);
        git(repo, &["rm", "-q", "-r", "dir"]);
        fs::write(repo.join("dir"), b"file-now\n").expect("write dir file");
        git(repo, &["add", "dir"]);
        run_with_identity(repo, &["commit", "-m", "df transition", "-q"]);
        git(repo, &["checkout", "-q", "main"]);
    };

    // (1) Ignored content under dir/ does not block: nested + plain ignored.
    {
        let upstream = root.join("ignored-upstream");
        let rust = root.join("ignored-rust");
        build(&upstream);
        build(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join(".gitignore"), b"*.log\n").expect("write .gitignore");
            fs::write(repo.join("dir").join("ignored.log"), b"junk\n").expect("write ignored log");
            fs::create_dir_all(repo.join("dir").join("nested")).expect("mkdir nested");
            fs::write(repo.join("dir").join("nested").join("cache.log"), b"deep\n")
                .expect("write nested ignored log");
        }

        let args = ["checkout", "side"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        assert!(
            expected.status.success(),
            "ignored files under the replaced directory must not block (git said: {})",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(rust.join("dir")).expect("read replaced dir"),
            b"file-now\n",
            "sley must replace the directory with the file"
        );
    }

    // (2) Control: untracked non-ignored content still blocks with exit 128.
    {
        let upstream = root.join("untracked-upstream");
        let rust = root.join("untracked-rust");
        build(&upstream);
        build(&rust);
        for repo in [&upstream, &rust] {
            fs::write(repo.join("dir").join("untracked.txt"), b"precious\n")
                .expect("write untracked file");
        }

        let args = ["checkout", "side"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        assert_eq!(
            expected.status.code(),
            Some(1),
            "untracked non-ignored files must block the checkout (git stderr: {})",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(rust.join("dir").join("untracked.txt")).expect("read untracked file"),
            b"precious\n",
            "blocking untracked file must survive"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

/// A linked worktree's `.git` is a gitfile, while `info/exclude` remains in the
/// common git directory. D/F safety checks must therefore resolve the common
/// directory instead of looking below the worktree's `.git` path.
#[test]
fn checkout_df_transition_honors_common_info_exclude_in_linked_worktree() {
    let root = unique_temp_dir("checkout-df-linked-info-exclude");

    let build = |repo: &Path, linked: &Path| {
        fs::create_dir_all(repo).expect("create repo dir");
        git(repo, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(repo.join("dir")).expect("mkdir dir");
        fs::write(repo.join("dir/tracked.txt"), b"tracked\n").expect("write tracked file");
        git(repo, &["add", "."]);
        run_with_identity(repo, &["commit", "-m", "base", "-q"]);

        git(repo, &["checkout", "-q", "-b", "side"]);
        git(repo, &["rm", "-q", "-r", "dir"]);
        fs::write(repo.join("dir"), b"file-now\n").expect("write replacement file");
        git(repo, &["add", "dir"]);
        run_with_identity(repo, &["commit", "-m", "df transition", "-q"]);
        git(repo, &["checkout", "-q", "main"]);

        let linked_arg = linked.to_str().expect("linked worktree path utf8");
        git(
            repo,
            &["worktree", "add", "-q", "--detach", linked_arg, "main"],
        );
        fs::write(repo.join(".git/info/exclude"), b"*.log\n").expect("write common exclude");
        fs::write(linked.join("dir/cache.log"), b"ignored\n").expect("write ignored file");
    };

    let upstream_repo = root.join("upstream-repo");
    let upstream_linked = root.join("upstream-linked");
    let rust_repo = root.join("rust-repo");
    let rust_linked = root.join("rust-linked");
    build(&upstream_repo, &upstream_linked);
    build(&rust_repo, &rust_linked);

    let args = ["checkout", "side"];
    let expected = run_output(sley_testkit::oracle_git(), &upstream_linked, &args);
    assert!(
        expected.status.success(),
        "oracle must honor common info/exclude: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let actual = run_output(sley_testkit::sley_bin!(), &rust_linked, &args);
    assert_same_output(actual, expected, &args);
    assert_eq!(
        fs::read(rust_linked.join("dir")).expect("read replacement file"),
        b"file-now\n"
    );

    let _ = fs::remove_dir_all(&root);
}

/// S11 regression (discriminating): `checkout -B <branch> <start>` must see the
/// full effective config cascade. The filter driver here is defined ONLY in
/// `$GIT_DIR/config.worktree` (`extensions.worktreeConfig = true`) — a layer a
/// repo-only read misses and one the filter engine's global fallback never
/// consults — so the branch-reset path must smudge exactly like upstream.
#[test]
fn checkout_branch_reset_sees_config_worktree_layer() {
    let root = unique_temp_dir("checkout-b-config-worktree");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    for repo in [&upstream, &rust] {
        fs::create_dir_all(repo).expect("create repo dir");
        git(repo, &["init", "-q", "-b", "main"]);
        fs::write(
            repo.join(".git").join("config"),
            "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n[extensions]\n\tworktreeConfig = true\n",
        )
        .expect("enable extensions.worktreeConfig");
        // The filter driver lives ONLY in the worktree-scoped config layer.
        fs::write(
            repo.join(".git").join("config.worktree"),
            "[filter \"upper\"]\n\tsmudge = tr a-z A-Z\n\tclean = tr A-Z a-z\n",
        )
        .expect("write config.worktree");
        fs::write(repo.join(".gitattributes"), b"hello.txt filter=upper\n")
            .expect("write .gitattributes");
        fs::write(repo.join("hello.txt"), b"base\n").expect("write base file");
        git(repo, &["add", ".gitattributes", "hello.txt"]);
        run_with_identity(repo, &["commit", "-m", "base", "-q"]);
        fs::write(repo.join("hello.txt"), b"main\n").expect("write main file");
        git(repo, &["add", "hello.txt"]);
        run_with_identity(repo, &["commit", "-m", "main", "-q"]);
    }

    let base_oid = String::from_utf8(git(&upstream, &["rev-parse", "HEAD~"]))
        .expect("base oid utf8")
        .trim()
        .to_string();

    // Quiet mode: keeps stdout empty on both sides so the assertion isolates
    // the filter cascade (the smudged file content) from unrelated
    // change-summary rendering differences.
    let args = ["checkout", "-q", "-B", "topic", base_oid.as_str()];
    let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
    assert!(
        expected.status.success(),
        "oracle -B failed: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let actual = run_output(sley_testkit::sley_bin!(), &rust, &args);
    assert_same_output(actual, expected, &args);

    assert_eq!(
        fs::read(rust.join("hello.txt")).expect("read sley hello.txt"),
        b"BASE\n",
        "sley ignored $GIT_DIR/config.worktree on the -B branch-reset path"
    );
    assert_eq!(
        fs::read(upstream.join("hello.txt")).expect("read git hello.txt"),
        b"BASE\n",
        "oracle fixture sanity: expected smudged content"
    );

    let _ = fs::remove_dir_all(&root);
}
