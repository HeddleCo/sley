//! Differential interop tests for `git merge` against the system `git` binary.
//!
//! Strategy: build a fixture with real `git` (fixed identity + dates so object
//! ids are deterministic), copy the repo, then run `sley merge` in one copy and
//! real `git merge` in the other and compare the substantive results (merge
//! commit oid, worktree bytes, index conflict stages, exit codes). git's merge
//! stdout includes a diffstat that sley does not emit yet, so stdout is checked
//! loosely (contains) rather than byte-for-byte.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use filetime::FileTime;

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).expect("create temp root");
    path
}

/// Run a program with a fixed, deterministic git identity + timestamp so commit
/// object ids are reproducible across `git` and `sley`.
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

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::oracle_git(), cwd, args)
}

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sley(cwd: &Path, args: &[&str]) -> Output {
    run_env(sley_testkit::sley_bin!(), cwd, args)
}

fn git_available() -> bool {
    Command::new(sley_testkit::oracle_git())
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn write_file(cwd: &Path, name: &str, content: &str) {
    fs::write(cwd.join(name), content).expect("write file");
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        if entry.file_name().to_string_lossy().ends_with(".lock") {
            continue;
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            copy_dir_all(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap_or_else(|err| {
                panic!("copy file {} -> {}: {err}", from.display(), to.display())
            });
        }
    }
}

fn head(cwd: &Path) -> String {
    String::from_utf8_lossy(&git(cwd, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string()
}

/// Build a repo on the default branch with two divergent branches:
/// base has a.txt + b.txt; `feature` changes b.txt; default branch changes a.txt
/// (non-overlapping → clean 3-way merge).
fn setup_clean(dir: &Path) {
    git_ok(
        dir.parent().unwrap_or(dir),
        &[
            "init",
            "-q",
            dir.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(dir, "a.txt", "a1\na2\na3\n");
    write_file(dir, "b.txt", "b1\nb2\nb3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "base"]);
    git_ok(dir, &["checkout", "-q", "-b", "feature"]);
    write_file(dir, "b.txt", "b1\nBETA\nb3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "feat"]);
    // back to the original branch (main or master)
    let default = default_branch(dir);
    git_ok(dir, &["checkout", "-q", &default]);
    write_file(dir, "a.txt", "a1\nALPHA\na3\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "mainwork"]);
}

/// Build a repo whose current branch is explicitly `main` and where `side` is a
/// fast-forward candidate from that branch.
fn setup_fast_forward_main(dir: &Path) {
    git_ok(
        dir.parent().unwrap_or(dir),
        &[
            "init",
            "-q",
            "-b",
            "main",
            dir.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(dir, "base.txt", "base\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "base"]);
    git_ok(dir, &["checkout", "-q", "-b", "side"]);
    write_file(dir, "side.txt", "side\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "side"]);
    git_ok(dir, &["checkout", "-q", "main"]);
}

fn default_branch(dir: &Path) -> String {
    // Whichever of main/master currently has commits.
    for name in ["main", "master"] {
        if git(dir, &["rev-parse", "--verify", name]).status.success() {
            return name.to_string();
        }
    }
    "main".to_string()
}

#[test]
fn sequential_clean_merges_restore_sparse_index_between_operations() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-sequential-sparse-index");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            reference.to_str().expect("UTF-8 test repository path"),
        ],
    );
    fs::create_dir_all(reference.join("deep")).expect("create in-cone directory");
    fs::create_dir_all(reference.join("outside1")).expect("create first sparse directory");
    fs::create_dir_all(reference.join("outside2")).expect("create second sparse directory");
    write_file(&reference, "deep/a", "base\n");
    write_file(&reference, "outside1/a", "base one\n");
    write_file(&reference, "outside2/a", "base two\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);

    git_ok(&reference, &["checkout", "-q", "-b", "update-one"]);
    write_file(&reference, "outside1/a", "updated one\n");
    git_ok(&reference, &["commit", "-qam", "update one"]);
    git_ok(&reference, &["checkout", "-q", "main"]);
    git_ok(&reference, &["checkout", "-q", "-b", "update-two"]);
    write_file(&reference, "outside2/a", "updated two\n");
    git_ok(&reference, &["commit", "-qam", "update two"]);
    git_ok(&reference, &["checkout", "-q", "main"]);
    write_file(&reference, "deep/a", "main\n");
    git_ok(&reference, &["commit", "-qam", "main"]);
    git_ok(
        &reference,
        &["sparse-checkout", "init", "--cone", "--sparse-index"],
    );
    git_ok(&reference, &["sparse-checkout", "set", "deep"]);
    copy_dir_all(&reference, &candidate);

    for branch in ["update-one", "update-two"] {
        let expected = git(&reference, &["merge", "-m", "merge", branch]);
        let trace = root.join(format!("{branch}.trace.json"));
        let actual = Command::new(sley_testkit::sley_bin!())
            .current_dir(&candidate)
            .args(["merge", "-m", "merge", branch])
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .env("GIT_AUTHOR_DATE", "@1790000000 -0500")
            .env("GIT_COMMITTER_DATE", "@1790000000 -0500")
            .env("GIT_TRACE2_EVENT", &trace)
            .output()
            .expect("run traced sley merge");
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "merge status differed for {branch}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert!(
            !fs::read_to_string(trace)
                .expect("read merge trace")
                .contains("ensure_full_index"),
            "clean sparse merge must not advertise a full-index expansion"
        );
        assert_eq!(
            git(&candidate, &["ls-files", "--sparse", "--stage"]).stdout,
            git(&reference, &["ls-files", "--sparse", "--stage"]).stdout,
            "sparse index differed after merging {branch}"
        );
        assert!(!candidate.join("outside1").exists());
        assert!(!candidate.join("outside2").exists());
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_previous_branch_with_ancestry_suffix_names_branch_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-previous-branch-suffix");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            reference.to_str().expect("UTF-8 test repository path"),
        ],
    );
    write_file(&reference, "base", "base\n");
    git_ok(&reference, &["add", "base"]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    git_ok(&reference, &["branch", "other"]);
    write_file(&reference, "main", "main\n");
    git_ok(&reference, &["add", "main"]);
    git_ok(&reference, &["commit", "-qm", "main"]);
    git_ok(&reference, &["checkout", "-q", "other"]);
    for (path, subject) in [("other-one", "other one"), ("other-two", "other two")] {
        write_file(&reference, path, subject);
        git_ok(&reference, &["add", path]);
        git_ok(&reference, &["commit", "-qm", subject]);
    }
    git_ok(&reference, &["checkout", "-q", "main"]);
    copy_dir_all(&reference, &candidate);

    let expected = git(&reference, &["merge", "@{-1}~1"]);
    let actual = sley(&candidate, &["merge", "@{-1}~1"]);
    assert!(expected.status.success());
    assert!(
        actual.status.success(),
        "Sley merge failed: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        git(&candidate, &["cat-file", "commit", "HEAD"]).stdout,
        git(&reference, &["cat-file", "commit", "HEAD"]).stdout,
        "merge commit bytes differed"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn merge_branch_mergeoptions_malformed_on_main_fails_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-mergeoptions-bad-main");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    setup_fast_forward_main(&reference);
    git_ok(&reference, &["config", "branch.main.mergeoptions", "'"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "main"]);
    let rs_out = sley(&candidate, &["merge", "main"]);

    assert_eq!(ref_out.status.code(), Some(128));
    assert_eq!(rs_out.status.code(), Some(128));
    assert_eq!(
        String::from_utf8_lossy(&rs_out.stderr),
        String::from_utf8_lossy(&ref_out.stderr),
        "malformed mergeoptions stderr differed"
    );
    assert_eq!(head(&candidate), head(&reference), "HEAD moved on failure");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_branch_mergeoptions_are_prepended_before_cli_args() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-mergeoptions-precedence");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    setup_fast_forward_main(&reference);
    git_ok(
        &reference,
        &["config", "branch.main.mergeoptions", "--ff-only"],
    );
    copy_dir_all(&reference, &candidate);

    let args = ["merge", "side", "--no-ff", "-m", "manual merge"];
    let ref_out = git(&reference, &args);
    let rs_out = sley(&candidate, &args);

    assert!(
        ref_out.status.success(),
        "git merge failed: {}",
        String::from_utf8_lossy(&ref_out.stderr)
    );
    assert!(
        rs_out.status.success(),
        "sley merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    assert_eq!(
        git(&candidate, &["rev-list", "--parents", "-1", "HEAD"]).stdout,
        git(&reference, &["rev-list", "--parents", "-1", "HEAD"]).stdout,
        "merge parent list differed from git"
    );
    assert_eq!(
        head(&candidate),
        head(&reference),
        "merge commit oid differed from git"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_clean_threeway_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-clean");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    setup_clean(&reference);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(
        &reference,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );
    let rs_out = sley(
        &candidate,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );

    assert!(ref_out.status.success(), "git merge failed");
    assert!(
        rs_out.status.success(),
        "sley merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // The merge commit object must be byte-identical (same tree, parents,
    // identity, message) → identical oid.
    assert_eq!(
        head(&candidate),
        head(&reference),
        "merge commit oid differs from git"
    );
    // Two parents, second is the feature tip.
    assert_eq!(
        git(&candidate, &["rev-parse", "HEAD^2"]).stdout,
        git(&reference, &["rev-parse", "HEAD^2"]).stdout
    );
    // Worktree content matches.
    assert_eq!(
        fs::read(candidate.join("a.txt")).expect("test operation should succeed"),
        fs::read(reference.join("a.txt")).expect("test operation should succeed")
    );
    assert_eq!(
        fs::read(candidate.join("b.txt")).expect("test operation should succeed"),
        fs::read(reference.join("b.txt")).expect("test operation should succeed")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_fast_forward_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-ff");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "f.txt", "one\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "c1"]);
    git_ok(&reference, &["checkout", "-q", "-b", "feature"]);
    write_file(&reference, "f.txt", "one\ntwo\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "c2"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "feature"]);
    let rs_out = sley(&candidate, &["merge", "feature"]);
    assert!(ref_out.status.success());
    assert!(
        rs_out.status.success(),
        "sley ff merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    assert_eq!(
        head(&candidate),
        head(&reference),
        "ff HEAD differs from git"
    );
    assert!(
        String::from_utf8_lossy(&rs_out.stdout).contains("Fast-forward"),
        "expected Fast-forward in output, got: {}",
        String::from_utf8_lossy(&rs_out.stdout)
    );
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        fs::read(reference.join("f.txt")).expect("test operation should succeed")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_already_up_to_date_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-utd");
    let repo = root.join("repo");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "one\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "c1"]);
    git_ok(&repo, &["checkout", "-q", "-b", "old", "HEAD"]);
    let default = default_branch(&repo);
    git_ok(&repo, &["checkout", "-q", &default]);
    write_file(&repo, "f.txt", "one\ntwo\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "c2"]);

    // Merging an ancestor ("old") is a no-op.
    let rs_out = sley(&repo, &["merge", "old"]);
    assert!(rs_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&rs_out.stdout).trim(),
        "Already up to date."
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            reference.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&reference, "x.txt", "1\n2\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);
    git_ok(&reference, &["checkout", "-q", "-b", "feature"]);
    write_file(&reference, "x.txt", "1\nFEATURE\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "feat"]);
    let default = default_branch(&reference);
    git_ok(&reference, &["checkout", "-q", &default]);
    write_file(&reference, "x.txt", "1\nMAIN\n3\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "mainwork"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "-m", "M", "feature"]);
    let rs_out = sley(&candidate, &["merge", "-m", "M", "feature"]);

    // Both fail with the same conflict exit code.
    assert_eq!(ref_out.status.code(), Some(1));
    assert_eq!(
        rs_out.status.code(),
        Some(1),
        "sley merge conflict exit differs: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // Conflicted working-tree bytes (markers) must match git exactly.
    assert_eq!(
        fs::read(candidate.join("x.txt")).expect("test operation should succeed"),
        fs::read(reference.join("x.txt")).expect("test operation should succeed"),
        "conflict markers differ from git"
    );
    // Index conflict stages must match git.
    assert_eq!(
        git(&candidate, &["ls-files", "-u"]).stdout,
        git(&reference, &["ls-files", "-u"]).stdout,
        "unmerged index stages differ from git"
    );
    // MERGE_HEAD recorded.
    assert!(
        git(&candidate, &["rev-parse", "--verify", "MERGE_HEAD"])
            .status
            .success(),
        "sley did not record MERGE_HEAD"
    );
    assert_eq!(
        git(&candidate, &["rev-parse", "--verify", "AUTO_MERGE"]).stdout,
        git(&reference, &["rev-parse", "--verify", "AUTO_MERGE"]).stdout,
        "AUTO_MERGE tree differs from git"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_abort_restores_pre_merge_state() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-abort");
    let repo = root.join("repo");
    git_ok(
        root.as_path(),
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "x.txt", "1\n2\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["checkout", "-q", "-b", "feature"]);
    write_file(&repo, "x.txt", "1\nFEATURE\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "feat"]);
    let default = default_branch(&repo);
    git_ok(&repo, &["checkout", "-q", &default]);
    write_file(&repo, "x.txt", "1\nMAIN\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "mainwork"]);
    let pre_head = head(&repo);

    let conflict = sley(&repo, &["merge", "-m", "M", "feature"]);
    assert_eq!(conflict.status.code(), Some(1));

    let abort = sley(&repo, &["merge", "--abort"]);
    assert!(
        abort.status.success(),
        "sley merge --abort failed: {}",
        String::from_utf8_lossy(&abort.stderr)
    );
    assert_eq!(head(&repo), pre_head, "HEAD moved after abort");
    // Working tree restored to ours; no merge state left.
    assert_eq!(
        fs::read(repo.join("x.txt")).expect("test operation should succeed"),
        b"1\nMAIN\n3\n"
    );
    assert!(
        !git(&repo, &["rev-parse", "--verify", "MERGE_HEAD"])
            .status
            .success(),
        "MERGE_HEAD still present after abort"
    );
    assert!(
        !git(&repo, &["rev-parse", "--verify", "AUTO_MERGE"])
            .status
            .success(),
        "AUTO_MERGE still present after abort"
    );
    // Index is clean (no unmerged entries).
    assert!(
        git(&repo, &["ls-files", "-u"]).stdout.is_empty(),
        "unmerged entries remain after abort"
    );

    fs::remove_dir_all(&root).ok();
}

fn setup_directory_submodule_conflict(repo: &Path) {
    git_ok(
        repo.parent().unwrap_or(repo),
        &[
            "init",
            "-q",
            "-b",
            "main",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    git_ok(repo, &["commit", "--allow-empty", "-qm", "O"]);
    for branch in ["A", "B1", "B2"] {
        git_ok(repo, &["branch", branch]);
    }

    git_ok(repo, &["checkout", "-q", "B1"]);
    fs::create_dir_all(repo.join("path")).expect("create B1 path dir");
    write_file(&repo.join("path"), "file", "contents\n");
    git_ok(repo, &["add", "path/file"]);
    git_ok(repo, &["commit", "-qm", "B1"]);

    git_ok(repo, &["checkout", "-q", "B2"]);
    fs::create_dir_all(repo.join("path")).expect("create B2 path dir");
    write_file(&repo.join("path"), "world", "contents\n");
    git_ok(repo, &["add", "path/world"]);
    git_ok(repo, &["commit", "-qm", "B2"]);

    git_ok(repo, &["checkout", "-q", "A"]);
    git_ok(repo, &["init", "-q", "-b", "main", "path"]);
    write_file(&repo.join("path"), "world", "hello\n");
    git_ok(&repo.join("path"), &["add", "world"]);
    git_ok(&repo.join("path"), &["commit", "-qm", "hello"]);
    git_ok(
        repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            "./path",
        ],
    );
    git_ok(repo, &["commit", "-qm", "A"]);
}

#[test]
fn merge_directory_submodule_conflict_keeps_submodule_clean_and_abort_works() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-directory-submodule-conflict");
    let repo = root.join("repo");
    setup_directory_submodule_conflict(&repo);

    let b1 = sley(&repo, &["merge", "B1"]);
    assert_eq!(
        b1.status.code(),
        Some(1),
        "Sley should report a directory/submodule conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&b1.stdout),
        String::from_utf8_lossy(&b1.stderr)
    );
    assert!(
        repo.join("path/.git").exists(),
        "path/ should remain a populated submodule"
    );
    assert!(
        !repo.join("path/file").exists(),
        "merge wrote B1's file into the submodule checkout"
    );
    let unmerged =
        String::from_utf8(git(&repo, &["ls-files", "-u"]).stdout).expect("ls-files -u utf8");
    assert_eq!(
        unmerged.lines().count(),
        1,
        "B1 conflict should leave only the gitlink unmerged"
    );
    let listed =
        String::from_utf8(git(&repo, &["ls-files", "-co"]).stdout).expect("ls-files -co utf8");
    let aside_has_contents = listed.lines().any(|path| {
        !path.starts_with("path/")
            && fs::read(repo.join(path))
                .map(|bytes| bytes == b"contents\n")
                .unwrap_or(false)
    });
    assert!(
        aside_has_contents,
        "B1's path/file content should be materialized outside the submodule"
    );
    assert!(
        git(&repo.join("path"), &["status", "--short"])
            .stdout
            .is_empty(),
        "B1 conflict dirtied the submodule checkout"
    );
    let abort_b1 = sley(&repo, &["merge", "--abort"]);
    assert!(
        abort_b1.status.success(),
        "Sley merge --abort after B1 failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&abort_b1.stdout),
        String::from_utf8_lossy(&abort_b1.stderr)
    );

    let b2 = sley(&repo, &["merge", "B2"]);
    assert_eq!(
        b2.status.code(),
        Some(1),
        "Sley should report a directory/submodule conflict for B2\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&b2.stdout),
        String::from_utf8_lossy(&b2.stderr)
    );
    assert!(
        repo.join(".git/MERGE_HEAD").is_file(),
        "MERGE_HEAD should exist during the conflicted merge"
    );
    assert_eq!(
        fs::read(repo.join("path/world")).expect("read submodule file"),
        b"hello\n",
        "B2 merge should not overwrite the submodule's own file"
    );
    assert!(
        git(&repo.join("path"), &["status", "--short"])
            .stdout
            .is_empty(),
        "B2 conflict dirtied the submodule checkout before abort"
    );

    let abort_b2 = sley(&repo, &["merge", "--abort"]);
    assert!(
        abort_b2.status.success(),
        "Sley merge --abort after B2 failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&abort_b2.stdout),
        String::from_utf8_lossy(&abort_b2.stderr)
    );
    assert!(
        !repo.join(".git/MERGE_HEAD").exists(),
        "MERGE_HEAD still present after abort"
    );
    assert!(
        git(&repo, &["ls-files", "-u"]).stdout.is_empty(),
        "unmerged entries remain after abort"
    );
    assert_eq!(
        fs::read(repo.join("path/world")).expect("read submodule file after abort"),
        b"hello\n",
        "abort should leave the submodule worktree intact"
    );
    assert!(
        git(&repo.join("path"), &["status", "--short"])
            .stdout
            .is_empty(),
        "abort dirtied the submodule checkout"
    );

    fs::remove_dir_all(&root).ok();
}

/// Build a "file renamed on the feature side, modified in place on the default
/// branch" fixture. Returns the default branch name. The merge should move the
/// default-branch modification onto the renamed destination (the merge-ort
/// non-recursive rename case).
fn setup_rename_merge(dir: &Path, theirs_change: &str, ours_change: &str) -> String {
    git_ok(
        dir.parent().unwrap_or(dir),
        &[
            "init",
            "-q",
            dir.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(dir, "old.txt", "1\n2\n3\n4\n5\n");
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "base"]);
    git_ok(dir, &["checkout", "-q", "-b", "feature"]);
    git_ok(dir, &["mv", "old.txt", "new.txt"]);
    write_file(dir, "new.txt", theirs_change);
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "rename+edit"]);
    let default = default_branch(dir);
    git_ok(dir, &["checkout", "-q", &default]);
    write_file(dir, "old.txt", ours_change);
    git_ok(dir, &["add", "."]);
    git_ok(dir, &["commit", "-qm", "edit-in-place"]);
    default
}

#[test]
fn merge_rename_clean_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-rename-clean");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    // The in-place edit touches a different region than the rename side, so the
    // 3-way content merge at the destination is clean.
    setup_rename_merge(&reference, "1\n2\n3\n4\nFIVE\n", "ONE\n2\n3\n4\n5\n");
    copy_dir_all(&reference, &candidate);

    let ref_out = git(
        &reference,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );
    let rs_out = sley(
        &candidate,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );

    assert!(ref_out.status.success(), "git rename merge failed");
    assert!(
        rs_out.status.success(),
        "sley rename merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    // The merge commit (and thus its tree) must be byte-identical to git: the
    // modification followed the rename, and old.txt is gone from the tree.
    assert_eq!(
        head(&candidate),
        head(&reference),
        "rename-merge commit oid differs from git"
    );
    // Result tree: only new.txt, carrying both edits.
    assert_eq!(
        git(&candidate, &["ls-tree", "-r", "--name-only", "HEAD"]).stdout,
        git(&reference, &["ls-tree", "-r", "--name-only", "HEAD"]).stdout,
        "rename-merge tree differs from git"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_rename_conflict_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-rename-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    // Both sides edit the same line: a content conflict, but it must be reported
    // at the renamed destination with all three stages (base/ours/theirs) at
    // new.txt, exactly like git's merge-ort.
    setup_rename_merge(&reference, "1\n2\nTHEIRS\n4\n5\n", "1\n2\nOURS\n4\n5\n");
    copy_dir_all(&reference, &candidate);

    let ref_out = git(
        &reference,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );
    let rs_out = sley(
        &candidate,
        &["merge", "-m", "Merge branch 'feature'", "feature"],
    );

    assert_eq!(ref_out.status.code(), Some(1), "git should conflict");
    assert_eq!(
        rs_out.status.code(),
        Some(1),
        "sley should conflict: {}",
        String::from_utf8_lossy(&rs_out.stdout)
    );
    // The unmerged index stages must match git byte-for-byte: three stages, all
    // at new.txt (the rename destination), with the same oids.
    assert_eq!(
        git(&candidate, &["ls-files", "-u"]).stdout,
        git(&reference, &["ls-files", "-u"]).stdout,
        "rename-conflict index stages differ from git"
    );
    // The conflicted worktree file lives at the destination with markers.
    assert_eq!(
        fs::read(candidate.join("new.txt")).expect("test operation should succeed"),
        fs::read(reference.join("new.txt")).expect("test operation should succeed"),
        "rename-conflict worktree content differs from git"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn merge_same_rename_conflict_keeps_common_ancestor_stage() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-same-rename-conflict");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            reference.to_str().expect("UTF-8 test repository path"),
        ],
    );
    write_file(&reference, "old.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);

    git_ok(&reference, &["checkout", "-q", "-b", "side"]);
    git_ok(&reference, &["mv", "old.txt", "new.txt"]);
    write_file(&reference, "new.txt", "1\n2\n3\nTHEIRS\n5\n6\n7\n8\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "side rename"]);

    git_ok(&reference, &["checkout", "-q", "main"]);
    git_ok(&reference, &["mv", "old.txt", "new.txt"]);
    write_file(&reference, "new.txt", "1\n2\n3\nOURS\n5\n6\n7\n8\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "main rename"]);
    copy_dir_all(&reference, &candidate);

    let ref_out = git(&reference, &["merge", "side"]);
    let rs_out = sley(&candidate, &["merge", "side"]);
    assert_eq!(ref_out.status.code(), Some(1));
    assert_eq!(rs_out.status.code(), Some(1));
    let expected_stages = git(&reference, &["ls-files", "-u", "new.txt"]).stdout;
    assert_eq!(expected_stages.split(|byte| *byte == b'\n').count() - 1, 3);
    assert_eq!(
        git(&candidate, &["ls-files", "-u", "new.txt"]).stdout,
        expected_stages,
        "same rename must relocate the base entry and retain stages 1/2/3"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn clean_directory_to_file_merge_preserves_unchanged_worktree_file() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("merge-df-unchanged-mtime");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            "-b",
            "main",
            reference.to_str().expect("UTF-8 test repository path"),
        ],
    );
    fs::create_dir_all(reference.join("df")).expect("create directory fixture");
    write_file(&reference, "df/file", "base\n");
    write_file(&reference, "irrelevant", "keep\n");
    git_ok(&reference, &["add", "."]);
    git_ok(&reference, &["commit", "-qm", "base"]);

    git_ok(&reference, &["checkout", "-q", "-b", "side"]);
    git_ok(&reference, &["rm", "-qr", "df"]);
    git_ok(&reference, &["commit", "-qm", "remove directory"]);

    git_ok(&reference, &["checkout", "-q", "main"]);
    git_ok(&reference, &["rm", "-qr", "df"]);
    write_file(&reference, "df", "main file\n");
    git_ok(&reference, &["add", "df"]);
    git_ok(
        &reference,
        &["commit", "-qm", "replace directory with file"],
    );
    copy_dir_all(&reference, &candidate);

    let old_mtime = FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(reference.join("df"), old_mtime).expect("set reference mtime");
    filetime::set_file_mtime(candidate.join("df"), old_mtime).expect("set candidate mtime");

    let ref_out = git(&reference, &["merge", "side"]);
    let rs_out = sley(&candidate, &["merge", "side"]);
    assert!(ref_out.status.success());
    assert!(
        rs_out.status.success(),
        "Sley merge failed: {}",
        String::from_utf8_lossy(&rs_out.stderr)
    );
    assert_eq!(
        fs::read(candidate.join("df")).expect("read merged path"),
        b"main file\n"
    );
    assert_eq!(
        FileTime::from_last_modification_time(
            &fs::metadata(candidate.join("df")).expect("stat merged path"),
        ),
        old_mtime,
        "an absent flattened child must not make D/F blocker cleanup unlink the surviving file"
    );

    fs::remove_dir_all(&root).ok();
}
