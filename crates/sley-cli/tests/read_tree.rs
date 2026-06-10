//! Differential tests for `git read-tree`, comparing the `sley` binary
//! against the system `git`.
//!
//! Each test stands up two identical repositories (one driven by upstream
//! `git`, one by `sley`), runs the same `read-tree` invocation in both, and
//! asserts the binaries agree on stdout, stderr, and exit status. Where the
//! command mutates the index it then re-reads each repo's resulting index with
//! the trusted system `git ls-files --stage`, so differences in transient
//! stat/cache metadata do not cause spurious failures — only the logical
//! `(mode, oid, stage, path)` content is compared. Worktree-affecting cases
//! (`-u`) additionally compare the on-disk file listing.
//!
//! The whole suite is skipped when no usable `git` is on `PATH`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

/// A unique scratch directory under the system temp dir.
fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

/// Whether a usable `git` binary is available; the suite no-ops otherwise.
fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The deterministic identity / date environment shared by both binaries so
/// commit and tree object ids are reproducible.
fn with_fixed_env(command: &mut Command) -> &mut Command {
    command
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
        .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
}

/// Run `program` in `cwd` capturing all output, with the fixed identity env.
fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    with_fixed_env(Command::new(program).current_dir(cwd).args(args))
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

/// Run a command that is expected to succeed, returning its stdout.
fn run_ok(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
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

/// Convenience wrapper for a system-`git` command expected to succeed.
fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run_ok(sley_testkit::oracle_git(), cwd, args)
}

/// Path to the `sley` binary under test.
fn git_rs() -> &'static str {
    env!("CARGO_BIN_EXE_sley")
}

/// Assert two command runs produced identical stdout, stderr, and exit code.
fn assert_same_output(actual: &Output, expected: &Output, args: &[&str]) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "exit status differed for {args:?}\n sley stderr:\n{}\n git stderr:\n{}",
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "stdout differed for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr),
        "stderr differed for {args:?}"
    );
}

/// The logical index content (`mode oid stage\tpath` lines), read back with the
/// trusted system `git` so transient cache metadata never affects the compare.
fn index_listing(repo: &Path) -> String {
    String::from_utf8(git(repo, &["ls-files", "--stage"])).expect("ls-files output is utf8")
}

/// The sorted list of tracked + untracked file paths in the worktree, used to
/// compare `-u` worktree updates.
fn worktree_files(repo: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(repo, repo, &mut files);
    files.sort();
    files
}

/// Recursively gather repository-relative file paths, skipping `.git`.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            collect_files(root, &path, out);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("path is under root")
                .to_string_lossy()
                .replace('\\', "/");
            out.push(relative);
        }
    }
}

/// Initialize an empty repository driven by either binary's `init`.
fn init_repo(program: &str, root: &Path) {
    fs::create_dir_all(root).expect("create repo dir");
    run_ok(program, root, &["init", "-q"]);
}

/// Build a two-commit history in `root` using the system `git` (object ids are
/// identical regardless of which binary later reads them):
///
/// * commit 1 (`tree1`): `a.txt` = "hello\n", `sub/b.txt` = "world\n"
/// * commit 2 (`tree2`): adds `c.txt` = "two\n"
///
/// Returns `(tree1, tree2)` object ids.
fn prepare_history(root: &Path) -> (String, String) {
    fs::write(root.join("a.txt"), b"hello\n").expect("write a.txt");
    fs::create_dir_all(root.join("sub")).expect("create sub");
    fs::write(root.join("sub").join("b.txt"), b"world\n").expect("write sub/b.txt");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
    let tree1 = oid_string(git(root, &["rev-parse", "HEAD^{tree}"]));

    fs::write(root.join("c.txt"), b"two\n").expect("write c.txt");
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "-q", "-m", "second"]);
    let tree2 = oid_string(git(root, &["rev-parse", "HEAD^{tree}"]));

    (tree1, tree2)
}

/// Trim a `rev-parse`-style stdout buffer to a bare object id string.
fn oid_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .expect("oid is utf8")
        .trim()
        .to_string()
}

/// Set up an identical two-commit history in both an upstream (`git`) and a
/// `sley` repository, returning `(upstream, rust, tree1, tree2)`.
///
/// The histories are created with the system `git` in both repos so commits and
/// trees share object ids; only the subsequent `read-tree` invocation differs.
fn paired_history(root: &Path) -> (PathBuf, PathBuf, String, String) {
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    init_repo(sley_testkit::oracle_git(), &upstream);
    init_repo(sley_testkit::oracle_git(), &rust);
    let (tree1, tree2) = prepare_history(&upstream);
    let (tree1_b, tree2_b) = prepare_history(&rust);
    assert_eq!(tree1, tree1_b, "fixture tree1 ids diverged");
    assert_eq!(tree2, tree2_b, "fixture tree2 ids diverged");
    (upstream, rust, tree1, tree2)
}

/// Run the same `read-tree` invocation in both repos and assert the binaries
/// agree on output *and* on the resulting index listing.
fn assert_read_tree_parity(upstream: &Path, rust: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), upstream, args);
    let actual = run_output(git_rs(), rust, args);
    assert_same_output(&actual, &expected, args);
    assert_eq!(
        index_listing(rust),
        index_listing(upstream),
        "index listing differed after {args:?}"
    );
}

#[test]
fn read_tree_single_tree_replaces_index() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-single");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        assert_read_tree_parity(&upstream, &rust, &["read-tree", &tree1]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_single_tree_replaces_index");
}

#[test]
fn read_tree_empty_tree_object_clears_to_that_tree() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-empty-tree");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, _t2) = paired_history(&root);
        // The canonical empty tree object id (sha1).
        let empty = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        assert_read_tree_parity(&upstream, &rust, &["read-tree", empty]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_empty_tree_object_clears_to_that_tree");
}

#[test]
fn read_tree_empty_flag_empties_index() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-empty-flag");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, _t2) = paired_history(&root);
        assert_read_tree_parity(&upstream, &rust, &["read-tree", "--empty"]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_empty_flag_empties_index");
}

#[test]
fn read_tree_no_arguments_warns_and_empties_index() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-noargs");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, _t2) = paired_history(&root);
        // No tree-ish: git emits the deprecation warning on stderr and empties
        // the index with exit 0.
        assert_read_tree_parity(&upstream, &rust, &["read-tree"]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_no_arguments_warns_and_empties_index");
}

#[test]
fn read_tree_two_trees_overlay_last_wins() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-overlay");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, tree2) = paired_history(&root);
        // Without -m, multiple trees union into stage 0 (later trees win).
        assert_read_tree_parity(&upstream, &rust, &["read-tree", &tree1, &tree2]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_two_trees_overlay_last_wins");
}

#[test]
fn read_tree_prefix_reads_tree_under_subdirectory() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-prefix");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        // --prefix overlays the tree under newdir/ keeping existing entries.
        assert_read_tree_parity(&upstream, &rust, &["read-tree", "--prefix=newdir/", &tree1]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_prefix_reads_tree_under_subdirectory");
}

#[test]
fn read_tree_prefix_without_trailing_slash_is_normalized() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-prefix-noslash");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        assert_read_tree_parity(&upstream, &rust, &["read-tree", "--prefix=zz", &tree1]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_prefix_without_trailing_slash_is_normalized");
}

#[test]
fn read_tree_reset_replaces_index_keeping_worktree() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-reset");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        // --reset (no -u) rewrites the index to tree1 but leaves the worktree
        // alone, so c.txt remains on disk in both repos.
        assert_read_tree_parity(&upstream, &rust, &["read-tree", "--reset", &tree1]);
        assert_eq!(
            worktree_files(&rust),
            worktree_files(&upstream),
            "worktree files differed after --reset"
        );
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_reset_replaces_index_keeping_worktree");
}

#[test]
fn read_tree_reset_update_resets_index_and_worktree() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-reset-u");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        // --reset -u rewrites the index AND the worktree to tree1, removing
        // c.txt from disk in both repos.
        let args = ["read-tree", "--reset", "-u", &tree1];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
        assert_eq!(
            index_listing(&rust),
            index_listing(&upstream),
            "index listing differed after --reset -u"
        );
        assert_eq!(
            worktree_files(&rust),
            worktree_files(&upstream),
            "worktree files differed after --reset -u"
        );
        // The removed file's contents agree (both should be gone).
        assert!(!rust.join("c.txt").exists(), "sley left c.txt on disk");
        assert!(!upstream.join("c.txt").exists(), "git left c.txt on disk");
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_reset_update_resets_index_and_worktree");
}

#[test]
fn read_tree_u_without_merge_mode_is_rejected() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-u-rejected");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        // -u is meaningless for a plain read; both binaries must reject it the
        // same way (fatal + exit 128) without disturbing the index.
        let args = ["read-tree", "-u", &tree1];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
        assert_eq!(
            index_listing(&rust),
            index_listing(&upstream),
            "index listing differed after rejected -u"
        );
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_u_without_merge_mode_is_rejected");
}

#[test]
fn read_tree_invalid_tree_ish_is_rejected() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-bad");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, _t2) = paired_history(&root);
        let args = ["read-tree", "definitely-not-a-real-object"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_invalid_tree_ish_is_rejected");
}

#[test]
fn read_tree_empty_with_tree_argument_conflicts() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-empty-conflict");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        let args = ["read-tree", "--empty", &tree1];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_empty_with_tree_argument_conflicts");
}

#[test]
fn read_tree_merge_and_prefix_together_conflict() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-m-prefix");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, tree1, _tree2) = paired_history(&root);
        // -m together with --prefix is rejected ("Which one?").
        let args = ["read-tree", "-m", "--prefix=x/", &tree1];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_merge_and_prefix_together_conflict");
}

#[test]
fn read_tree_merge_requires_a_tree() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-m-notree");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, _t2) = paired_history(&root);
        let args = ["read-tree", "-m"];
        let expected = run_output(sley_testkit::oracle_git(), &upstream, &args);
        let actual = run_output(git_rs(), &rust, &args);
        assert_same_output(&actual, &expected, &args);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_merge_requires_a_tree");
}

#[test]
fn read_tree_merge_one_tree_fast_forward_on_clean_worktree() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-m-ff");
    let result = std::panic::catch_unwind(|| {
        let (upstream, rust, _t1, tree2) = paired_history(&root);
        // Index + worktree already match tree2 (HEAD); a one-tree merge to the
        // same tree is a clean fast-forward in both binaries.
        assert_read_tree_parity(&upstream, &rust, &["read-tree", "-m", &tree2]);
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_merge_one_tree_fast_forward_on_clean_worktree");
}

#[test]
fn read_tree_three_way_merge_produces_matching_stages() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("read-tree-3way");
    let result = std::panic::catch_unwind(|| {
        let upstream = root.join("upstream");
        let rust = root.join("rust");
        init_repo(sley_testkit::oracle_git(), &upstream);
        init_repo(sley_testkit::oracle_git(), &rust);
        let (base, ours, theirs) = prepare_three_way_fixture(&upstream);
        let (base_b, ours_b, theirs_b) = prepare_three_way_fixture(&rust);
        assert_eq!((&base, &ours, &theirs), (&base_b, &ours_b, &theirs_b));

        // Put both repos in a clean state matching `ours` (index + worktree) so
        // the trivial three-way merge has no "not uptodate" objections.
        for repo in [&upstream, &rust] {
            git(repo, &["read-tree", "--reset", "-u", &ours]);
        }

        assert_read_tree_parity(
            &upstream,
            &rust,
            &["read-tree", "-m", &base, &ours, &theirs],
        );
    });
    let _ = fs::remove_dir_all(&root);
    result.expect("read_tree_three_way_merge_produces_matching_stages");
}

/// Build three trees exercising every trivial three-way merge outcome and
/// return their `(base, ours, theirs)` object ids.
///
/// Files (content per side, `-` = absent):
///
/// | path             | base | ours | theirs |
/// |------------------|------|------|--------|
/// | same_all         |  O   |  O   |   O    |
/// | ours_changed     |  O   |  A   |   O    |
/// | theirs_changed   |  O   |  O   |   B    |
/// | both_same        |  O   |  A   |   A    |
/// | both_diff        |  O   |  A   |   B    |
/// | add_ours         |  -   |  A   |   -    |
/// | add_theirs       |  -   |  -   |   B    |
/// | add_both_same    |  -   |  A   |   A    |
/// | add_both_diff    |  -   |  A   |   B    |
/// | del_ours         |  O   |  -   |   O    |
/// | del_theirs       |  O   |  O   |   -    |
/// | del_both         |  O   |  -   |   -    |
fn prepare_three_way_fixture(root: &Path) -> (String, String, String) {
    let o = hash_blob(root, b"O\n");
    let a = hash_blob(root, b"A\n");
    let b = hash_blob(root, b"B\n");

    let base = make_tree(
        root,
        &[
            ("same_all", &o),
            ("ours_changed", &o),
            ("theirs_changed", &o),
            ("both_same", &o),
            ("both_diff", &o),
            ("del_ours", &o),
            ("del_theirs", &o),
            ("del_both", &o),
        ],
    );
    let ours = make_tree(
        root,
        &[
            ("same_all", &o),
            ("ours_changed", &a),
            ("theirs_changed", &o),
            ("both_same", &a),
            ("both_diff", &a),
            ("add_ours", &a),
            ("add_both_same", &a),
            ("add_both_diff", &a),
            ("del_theirs", &o),
        ],
    );
    let theirs = make_tree(
        root,
        &[
            ("same_all", &o),
            ("ours_changed", &o),
            ("theirs_changed", &b),
            ("both_same", &a),
            ("both_diff", &b),
            ("add_theirs", &b),
            ("add_both_same", &a),
            ("add_both_diff", &b),
            ("del_ours", &o),
        ],
    );
    (base, ours, theirs)
}

/// Hash `content` into the object store with `git hash-object -w`.
fn hash_blob(root: &Path, content: &[u8]) -> String {
    let mut child = with_fixed_env(Command::new(sley_testkit::oracle_git()).current_dir(root).args([
        "hash-object",
        "-w",
        "--stdin",
    ]))
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("spawn git hash-object");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("hash-object stdin is piped")
        .write_all(content)
        .expect("write hash-object stdin");
    let output = child.wait_with_output().expect("wait for hash-object");
    assert!(output.status.success(), "git hash-object failed");
    oid_string(output.stdout)
}

/// Build a flat tree object from `(name, blob_oid)` pairs via `git mktree`.
fn make_tree(root: &Path, entries: &[(&str, &str)]) -> String {
    let mut input = String::new();
    for (name, oid) in entries {
        input.push_str(&format!("100644 blob {oid}\t{name}\n"));
    }
    let mut child = with_fixed_env(Command::new(sley_testkit::oracle_git()).current_dir(root).arg("mktree"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git mktree");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("mktree stdin is piped")
        .write_all(input.as_bytes())
        .expect("write mktree stdin");
    let output = child.wait_with_output().expect("wait for mktree");
    assert!(
        output.status.success(),
        "git mktree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    oid_string(output.stdout)
}
