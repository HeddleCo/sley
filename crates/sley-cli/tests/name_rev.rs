//! Differential interop tests for `git name-rev` against the system `git`.
//!
//! Each case runs the same arguments through the reference `git` binary and the
//! `sley` build and asserts identical stdout, stderr, and exit code. A handful
//! of fixed env vars pin author/committer identity and dates so object ids (and
//! therefore names and any abbreviations) are reproducible. `--all` output order
//! is an unspecified hash-map order upstream, so those cases compare the sorted
//! set of lines instead of the raw byte stream.

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
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

fn run_env(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_env_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin)
        .expect("write stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_env(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Assert byte-identical stdout/stderr and matching exit code.
fn assert_same(cwd: &Path, args: &[&str]) {
    let expected = git(cwd, args);
    let actual = git_rs(cwd, args);
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differs for {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differs for {args:?}"
    );
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit differs for {args:?}"
    );
}

/// Like [`assert_same`] but with bytes fed on stdin.
fn assert_same_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) {
    let expected = run_env_stdin(sley_testkit::oracle_git(), cwd, args, stdin);
    let actual = run_env_stdin(env!("CARGO_BIN_EXE_sley"), cwd, args, stdin);
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differs for {args:?} (stdin)\nsley stderr: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differs for {args:?} (stdin)"
    );
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit differs for {args:?} (stdin)"
    );
}

/// `--all` lists commits in an unspecified order; compare the sorted line set.
fn assert_same_sorted(cwd: &Path, args: &[&str]) {
    let expected = git(cwd, args);
    let actual = git_rs(cwd, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit differs for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differs for {args:?}"
    );
    let mut expected_lines: Vec<&str> = std::str::from_utf8(&expected.stdout)
        .expect("git stdout utf8")
        .lines()
        .collect();
    let mut actual_lines: Vec<&str> = std::str::from_utf8(&actual.stdout)
        .expect("sley stdout utf8")
        .lines()
        .collect();
    expected_lines.sort_unstable();
    actual_lines.sort_unstable();
    assert_eq!(
        actual_lines,
        expected_lines,
        "sorted stdout differs for {args:?}\nsley stderr: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
}

fn commit_empty(cwd: &Path, message: &str) {
    git_ok(cwd, &["commit", "--allow-empty", "-qm", message]);
}

fn rev_parse(cwd: &Path, rev: &str) -> String {
    let output = git(cwd, &["rev-parse", rev]);
    assert!(
        output.status.success(),
        "rev-parse {rev} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("rev-parse output utf8")
        .trim()
        .to_string()
}

/// Build a fixture with a tagged linear history plus a feature branch merged
/// back into `main`, exercising `~`/`^N` naming, tags, and branches.
///
/// ```text
///   *  merge   (main)
///   |\
///   | * f2     (feature)
///   | * f1
///   * | c4
///   * | c3
///   |/
///   * c2       (tags/v1 on c3 below — see code)
///   * c1
/// ```
fn build_fixture(repo: &Path) {
    git_ok(repo, &["init", "-q", "-b", "main"]);
    commit_empty(repo, "c1");
    commit_empty(repo, "c2");
    commit_empty(repo, "c3");
    git_ok(repo, &["tag", "v1"]);
    commit_empty(repo, "c4");
    git_ok(repo, &["checkout", "-q", "-b", "feature", "HEAD~2"]);
    commit_empty(repo, "f1");
    commit_empty(repo, "f2");
    git_ok(repo, &["checkout", "-q", "main"]);
    git_ok(
        repo,
        &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"],
    );
}

#[test]
fn name_rev_basic_branches_and_tags_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-basic");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    build_fixture(&repo);

    let merge = rev_parse(&repo, "HEAD");
    let second_parent = rev_parse(&repo, "HEAD^2");
    let v1 = rev_parse(&repo, "v1");
    // f1: reachable as `feature~1` and (further) `main^2~1` — exercises the
    // second-parent-then-first-parent suffix and the closest-tip preference.
    let feature_f1 = rev_parse(&repo, "HEAD^2~1");
    // A deep first-parent commit named off the tag chain (`tags/v1~1`).
    let deep = rev_parse(&repo, "HEAD~3");

    for args in [
        vec!["name-rev", "HEAD"],
        vec!["name-rev", "HEAD~1"],
        vec!["name-rev", "HEAD^2"],
        vec!["name-rev", merge.as_str()],
        vec!["name-rev", second_parent.as_str()],
        vec!["name-rev", v1.as_str()],
        vec!["name-rev", feature_f1.as_str()],
        vec!["name-rev", deep.as_str()],
        vec!["name-rev", "HEAD", "HEAD~1", second_parent.as_str()],
        vec!["name-rev", "--name-only", "HEAD"],
        vec!["name-rev", "--name-only", second_parent.as_str()],
        vec!["name-rev", "--tags", "HEAD"],
        vec!["name-rev", "--tags", v1.as_str()],
        vec!["name-rev", "--tags", "--name-only", v1.as_str()],
        vec![
            "name-rev",
            "--tags",
            "--name-only",
            rev_parse(&repo, "v1~1").as_str(),
        ],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_refs_and_exclude_filters_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-refs");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    build_fixture(&repo);

    let merge = rev_parse(&repo, "HEAD");
    let second_parent = rev_parse(&repo, "HEAD^2");
    let v1 = rev_parse(&repo, "v1");

    for args in [
        vec!["name-rev", "--refs=refs/heads/*", merge.as_str()],
        vec!["name-rev", "--refs=refs/tags/*", v1.as_str()],
        vec!["name-rev", "--refs=refs/tags/*", merge.as_str()],
        vec!["name-rev", "--refs=v1", v1.as_str()],
        vec!["name-rev", "--refs=tags/v1", v1.as_str()],
        vec!["name-rev", "--refs=*feature*", second_parent.as_str()],
        vec!["name-rev", "--refs=feature", second_parent.as_str()],
        vec![
            "name-rev",
            "--refs=refs/heads/feature",
            "--refs=refs/tags/*",
            second_parent.as_str(),
        ],
        vec![
            "name-rev",
            "--refs=refs/tags/*",
            "--no-refs",
            merge.as_str(),
        ],
        vec![
            "name-rev",
            "--exclude=refs/heads/feature",
            second_parent.as_str(),
        ],
        vec![
            "name-rev",
            "--refs=refs/heads/*",
            "--exclude=*feature*",
            second_parent.as_str(),
        ],
        vec!["name-rev", "--refs=refs/tags/none", merge.as_str()],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_annotated_tag_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-anno");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    git_ok(&repo, &["init", "-q", "-b", "main"]);
    commit_empty(&repo, "root");
    git_ok(&repo, &["checkout", "-q", "-b", "side"]);
    commit_empty(&repo, "s1");
    git_ok(&repo, &["checkout", "-q", "main"]);
    commit_empty(&repo, "m1");
    git_ok(&repo, &["merge", "-q", "--no-ff", "side", "-m", "MERGE"]);
    git_ok(&repo, &["tag", "-a", "-m", "tag on merge", "tm"]);

    let merge = rev_parse(&repo, "HEAD");
    let first_parent = rev_parse(&repo, "HEAD^1");
    let second_parent = rev_parse(&repo, "HEAD^2");
    let tag_oid = rev_parse(&repo, "refs/tags/tm");

    for args in [
        vec!["name-rev", merge.as_str()],
        vec!["name-rev", first_parent.as_str()],
        vec!["name-rev", second_parent.as_str()],
        vec!["name-rev", "--refs=tm", merge.as_str()],
        vec!["name-rev", "--refs=refs/tags/tm", merge.as_str()],
        vec!["name-rev", "--tags", "--name-only", merge.as_str()],
        // A tag object id resolves through to its commit's name via exact match.
        vec!["name-rev", tag_oid.as_str()],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_undefined_and_always_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-undef");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    git_ok(&repo, &["init", "-b", "main", "-q"]);
    commit_empty(&repo, "c1");
    git_ok(&repo, &["tag", "t1"]);
    commit_empty(&repo, "c2");
    // c2 is only reachable from main; drop main so it becomes unnamed.
    let c2 = rev_parse(&repo, "HEAD");
    git_ok(&repo, &["checkout", "-q", "--detach"]);
    git_ok(&repo, &["branch", "-D", "main"]);
    let tree = rev_parse(&repo, &format!("{c2}^{{tree}}"));

    for args in [
        vec!["name-rev", c2.as_str()],
        vec!["name-rev", "--tags", c2.as_str()],
        vec!["name-rev", "--always", c2.as_str()],
        vec!["name-rev", "--no-undefined", "--always", c2.as_str()],
        vec![
            "name-rev",
            "--no-undefined",
            "--always",
            "--name-only",
            c2.as_str(),
        ],
        vec!["name-rev", "--no-undefined", c2.as_str()],
        // A tree object is never a commit -> undefined, exit 0.
        vec!["name-rev", tree.as_str()],
        // Unresolvable names / ids print a "Skipping" notice on stderr, exit 0.
        vec!["name-rev", "definitely-not-a-ref"],
        vec!["name-rev", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"],
    ] {
        assert_same(&repo, &args);
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_all_matches_git_sorted() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-all");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    build_fixture(&repo);

    // `--all` lists every commit reachable from all refs (all refs are naming
    // tips here, so each gets a name). Output order is an unspecified hash-map
    // order upstream, hence the sorted comparison. `--all --tags` is omitted: its
    // listed-commit set depends on git's internal object-indexing during the run
    // rather than a documented rule.
    assert_same_sorted(&repo, &["name-rev", "--all"]);
    assert_same_sorted(&repo, &["name-rev", "--all", "--name-only"]);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_annotate_stdin_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-stdin");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    build_fixture(&repo);

    let head = rev_parse(&repo, "HEAD");
    let head1 = rev_parse(&repo, "HEAD~1");
    let short = &head[..10];

    let multi = format!("merge {head} and parent {head1}\nshort {short} stays\n");
    let embedded = format!("x{head} word{head}x bare {head} end\n");
    let upper = format!("U {}\n", head.to_uppercase());

    for stdin in [multi.as_bytes(), embedded.as_bytes(), upper.as_bytes()] {
        assert_same_stdin(&repo, &["name-rev", "--annotate-stdin"], stdin);
        assert_same_stdin(
            &repo,
            &["name-rev", "--annotate-stdin", "--name-only"],
            stdin,
        );
        // The deprecated alias must reproduce the warning on stderr too.
        assert_same_stdin(&repo, &["name-rev", "--stdin"], stdin);
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn name_rev_usage_errors_match_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("name-rev-usage");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    build_fixture(&repo);

    for args in [
        vec!["name-rev", "--all", "HEAD"],
        vec!["name-rev", "--bogus", "HEAD"],
        vec!["name-rev", "--refs"],
    ] {
        assert_same(&repo, &args);
    }
    // `--annotate-stdin` mixed with a positional rev is rejected before reading
    // stdin, so an empty stdin is fine here.
    assert_same_stdin(&repo, &["name-rev", "--annotate-stdin", "HEAD"], b"");

    fs::remove_dir_all(&root).ok();
}
