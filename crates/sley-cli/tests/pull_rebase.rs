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

fn prepare_diverged_upstream(upstream: &Path) {
    git(upstream, &["init", "-q", "-b", "master"]);
    prepare_identity(upstream);
    fs::write(upstream.join("shared.txt"), b"base\n").expect("write shared file");
    git(upstream, &["add", "shared.txt"]);
    git_with_identity(upstream, &["commit", "-m", "base", "-q"]);
    let base = String::from_utf8(
        run_output(sley_testkit::oracle_git(), upstream, &["rev-parse", "HEAD"]).stdout,
    )
    .expect("base oid utf8")
    .trim()
    .to_string();
    git(upstream, &["checkout", "-b", "topic", &base, "-q"]);
    fs::write(upstream.join("topic.txt"), b"topic-only\n").expect("write topic file");
    git(upstream, &["add", "topic.txt"]);
    git_with_identity(upstream, &["commit", "-m", "topic", "-q"]);
    git(upstream, &["checkout", "master", "-q"]);
    fs::write(upstream.join("main.txt"), b"main-only\n").expect("write main file");
    git(upstream, &["add", "main.txt"]);
    git_with_identity(upstream, &["commit", "-m", "main", "-q"]);
    git(upstream, &["checkout", "topic", "-q"]);
}

fn prepare_pull_rebase_clone(upstream: &Path, clone: &Path, rebase_config: Option<&str>) {
    prepare_diverged_upstream(clone);
    let upstream_arg = upstream.to_str().expect("upstream path is utf8");
    git(clone, &["remote", "add", "origin", upstream_arg]);
    git(clone, &["fetch", "origin", "-q"]);
    if let Some(value) = rebase_config {
        git(clone, &["config", "pull.rebase", value]);
    }
}

#[test]
fn pull_rebase_clean_matches_upstream_git() {
    let root = unique_temp_dir("pull-rebase-clean");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_diverged_upstream(&upstream);
    prepare_pull_rebase_clone(&upstream, &expected, Some("true"));
    prepare_pull_rebase_clone(&upstream, &actual, Some("true"));
    let args = ["pull", "origin", "master"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull rebase"
    );
    assert!(
        actual_output.status.success(),
        "sley pull --rebase failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after pull rebase"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &expected, &["log", "--oneline"]).stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["log", "--oneline"]).stdout,
        "log order differed after pull rebase"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pull_rebase_flag_matches_upstream_git() {
    let root = unique_temp_dir("pull-rebase-flag");
    let upstream = root.join("upstream");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    prepare_diverged_upstream(&upstream);
    prepare_pull_rebase_clone(&upstream, &expected, Some("false"));
    prepare_pull_rebase_clone(&upstream, &actual, Some("false"));
    let args = ["pull", "--rebase", "origin", "master"];
    let expected_output = run_output_with_identity(sley_testkit::oracle_git(), &expected, &args);
    let actual_output = run_output_with_identity(sley_testkit::sley_bin!(), &actual, &args);
    assert_eq!(
        actual_output.status.code(),
        expected_output.status.code(),
        "status differed for pull --rebase"
    );
    assert!(
        actual_output.status.success(),
        "sley pull --rebase failed: {}",
        String::from_utf8_lossy(&actual_output.stderr)
    );
    assert_eq!(
        run_output(
            sley_testkit::oracle_git(),
            &expected,
            &["rev-parse", "HEAD"]
        )
        .stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["rev-parse", "HEAD"]).stdout,
        "HEAD differed after pull --rebase"
    );
    assert_eq!(
        run_output(sley_testkit::oracle_git(), &expected, &["log", "--oneline"]).stdout,
        run_output(sley_testkit::sley_bin!(), &actual, &["log", "--oneline"]).stdout,
        "log order differed after pull --rebase"
    );
    let _ = fs::remove_dir_all(&root);
}

/// t5520 #61 / #62: `pull --rebase=interactive` / `--rebase=i` must open the
/// sequence editor (falling back to `GIT_EDITOR`) even when HEAD is already a
/// descendant of the upstream tip.
#[test]
fn pull_rebase_interactive_opens_sequence_editor() {
    let root = unique_temp_dir("pull-rebase-interactive");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    prepare_identity(&repo);
    fs::write(repo.join("file"), b"file\n").expect("write file");
    git(&repo, &["add", "file"]);
    git_with_identity(&repo, &["commit", "-m", "original", "-q"]);
    git(&repo, &["branch", "copy", "main"]);
    fs::write(repo.join("file"), b"updated\n").expect("write file");
    git_with_identity(&repo, &["commit", "-a", "-m", "updated", "-q"]);
    git(&repo, &["checkout", "copy", "-q"]);
    fs::write(repo.join("file2"), b"new\n").expect("write file2");
    git(&repo, &["add", "file2"]);
    git_with_identity(&repo, &["commit", "-m", "new file", "-q"]);
    // Leave HEAD strictly ahead of main so the pull is not a fast-forward.
    run_success(
        sley_testkit::oracle_git(),
        &repo,
        &["pull", "--rebase", ".", "main"],
    );

    let editor = root.join("fake-editor");
    fs::write(
        &editor,
        "#!/bin/sh\necho I was here >fake.out\nfalse\n",
    )
    .expect("write editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor, perms).expect("chmod");
    }
    let editor_str = editor.to_str().expect("utf8 path");

    for (label, rebase_arg) in [
        ("interactive", "--rebase=interactive"),
        ("i", "--rebase=i"),
    ] {
        let _ = fs::remove_file(repo.join("fake.out"));
        let _ = run_output(sley_testkit::sley_bin!(), &repo, &["rebase", "--abort"]);
        let output = Command::new(sley_testkit::sley_bin!())
            .current_dir(&repo)
            .args(["pull", rebase_arg, ".", "main"])
            .env("GIT_EDITOR", editor_str)
            .env("EDITOR", editor_str)
            .env_remove("GIT_SEQUENCE_EDITOR")
            .env("GIT_AUTHOR_NAME", "Example User")
            .env("GIT_AUTHOR_EMAIL", "example@example.invalid")
            .env("GIT_COMMITTER_NAME", "Example User")
            .env("GIT_COMMITTER_EMAIL", "example@example.invalid")
            .output()
            .expect("run sley pull interactive");
        assert!(
            !output.status.success(),
            "pull {label} should fail when the sequence editor returns false; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fake = fs::read_to_string(repo.join("fake.out")).unwrap_or_default();
        assert_eq!(
            fake.trim(),
            "I was here",
            "sequence editor was not launched for pull {label}"
        );
        let _ = run_output(sley_testkit::sley_bin!(), &repo, &["rebase", "--abort"]);
    }
    let _ = fs::remove_dir_all(&root);
}

/// t5520 #77: a local commit that matches an upstream patch by patch-id is
/// skipped rather than reapplied (no conflict).
#[test]
fn pull_rebase_detects_upstreamed_changes() {
    let root = unique_temp_dir("pull-rebase-upstreamed");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).expect("create src");
    git(&src, &["init", "-q", "-b", "main"]);
    prepare_identity(&src);
    fs::write(src.join("stuff"), b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").expect("write stuff");
    git(&src, &["add", "stuff"]);
    git_with_identity(&src, &["commit", "-m", "one", "-q"]);
    run_success(
        sley_testkit::oracle_git(),
        &root,
        &[
            "clone",
            "-q",
            src.to_str().expect("utf8"),
            dst.to_str().expect("utf8"),
        ],
    );
    prepare_identity(&dst);

    // Upstream discovers 5→43 then 6→42.
    let stuff = fs::read_to_string(src.join("stuff")).expect("read");
    fs::write(src.join("stuff"), stuff.replacen('5', "43", 1)).expect("write");
    git_with_identity(&src, &["commit", "-a", "-m", "5->43", "-q"]);
    let stuff = fs::read_to_string(src.join("stuff")).expect("read");
    fs::write(src.join("stuff"), stuff.replacen('6', "42", 1)).expect("write");
    git_with_identity(&src, &["commit", "-a", "-m", "Make it bigger", "-q"]);

    // Downstream independently discovers the same 5→43 change.
    let stuff = fs::read_to_string(dst.join("stuff")).expect("read");
    fs::write(dst.join("stuff"), stuff.replacen('5', "43", 1)).expect("write");
    git_with_identity(
        &dst,
        &["commit", "-a", "-m", "Independent discovery of 5->43", "-q"],
    );

    let output = run_output_with_identity(sley_testkit::sley_bin!(), &dst, &["pull", "--rebase"]);
    assert!(
        output.status.success(),
        "pull --rebase should skip the upstreamed patch; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let unmerged = run_output(sley_testkit::sley_bin!(), &dst, &["ls-files", "-u"]);
    assert!(
        unmerged.stdout.is_empty(),
        "expected no unmerged paths after detecting upstreamed change"
    );
    let head_subject = String::from_utf8_lossy(
        &run_output(
            sley_testkit::sley_bin!(),
            &dst,
            &["log", "-1", "--format=%s"],
        )
        .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(head_subject, "Make it bigger");
    let _ = fs::remove_dir_all(&root);
}

/// t5520 #79: after push + amend, `pull --rebase` only replays the amended
/// commit (one todo entry), not the already-pushed parents.
#[test]
fn pull_rebase_does_not_reapply_old_patches() {
    let root = unique_temp_dir("pull-rebase-no-reapply");
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(&src).expect("create src");
    git(&src, &["init", "-q", "-b", "main"]);
    prepare_identity(&src);
    fs::write(src.join("stuff"), b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").expect("write");
    git(&src, &["add", "stuff"]);
    git_with_identity(&src, &["commit", "-m", "one", "-q"]);
    // Bare remote so push does not fight a checked-out branch.
    let bare = root.join("src.git");
    run_success(
        sley_testkit::oracle_git(),
        &root,
        &[
            "clone",
            "--bare",
            "-q",
            src.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
    run_success(
        sley_testkit::oracle_git(),
        &root,
        &[
            "clone",
            "-q",
            bare.to_str().expect("utf8"),
            dst.to_str().expect("utf8"),
        ],
    );
    prepare_identity(&dst);

    let mut body = fs::read_to_string(dst.join("stuff")).expect("read");
    body = body.replacen('2', "22", 1);
    fs::write(dst.join("stuff"), &body).expect("write");
    git_with_identity(&dst, &["commit", "-a", "-m", "Change 2", "-q"]);
    body = body.replacen('3', "33", 1);
    fs::write(dst.join("stuff"), &body).expect("write");
    git_with_identity(&dst, &["commit", "-a", "-m", "Change 3", "-q"]);
    body = body.replacen('4', "44", 1);
    fs::write(dst.join("stuff"), &body).expect("write");
    git_with_identity(&dst, &["commit", "-a", "-m", "Change 4", "-q"]);
    git(&dst, &["push", "-q"]);
    body = body.replacen("44", "55", 1);
    fs::write(dst.join("stuff"), &body).expect("write");
    git_with_identity(
        &dst,
        &["commit", "--amend", "-a", "-m", "Modified Change 4", "-q"],
    );

    let output = run_output_with_identity(sley_testkit::sley_bin!(), &dst, &["pull", "--rebase"]);
    assert!(
        !output.status.success(),
        "amended tip should conflict when replayed onto the pre-amend push"
    );
    let done = fs::read_to_string(dst.join(".git/rebase-merge/done")).unwrap_or_default();
    let todo = fs::read_to_string(dst.join(".git/rebase-merge/git-rebase-todo")).unwrap_or_default();
    let combined = format!("{done}{todo}");
    let patches: Vec<&str> = combined
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    assert_eq!(
        patches.len(),
        1,
        "expected a single todo entry (amended tip only), got {patches:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// t5520 #80: `git pull --rebase . <local-branch>` rebases onto a local branch
/// without requiring a configured remote, skipping already-upstreamed patches.
#[test]
fn pull_rebase_against_local_branch() {
    let root = unique_temp_dir("pull-rebase-local");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    git(&repo, &["init", "-q", "-b", "main"]);
    prepare_identity(&repo);
    fs::write(repo.join("file"), b"file\n").expect("write");
    git(&repo, &["add", "file"]);
    git_with_identity(&repo, &["commit", "-m", "original", "-q"]);

    // Mirror t5520's `--rebase` + `--rebase with rebased upstream` setup.
    git(&repo, &["branch", "copy", "main"]);
    fs::write(repo.join("file"), b"updated\n").expect("write");
    git_with_identity(&repo, &["commit", "-a", "-m", "updated", "-q"]);
    // `copy` advances with "modified again"; `to-rebase` has a side commit.
    git(&repo, &["checkout", "copy", "-q"]);
    git(&repo, &["branch", "to-rebase"]);
    fs::write(repo.join("file"), b"modified again\n").expect("write");
    git_with_identity(&repo, &["commit", "-a", "-m", "file", "-q"]);
    git(&repo, &["checkout", "to-rebase", "-q"]);
    fs::write(repo.join("file2"), b"new\n").expect("write");
    git(&repo, &["add", "file2"]);
    git_with_identity(&repo, &["commit", "-m", "new file", "-q"]);
    run_success(
        sley_testkit::oracle_git(),
        &repo,
        &["pull", "--rebase", ".", "copy"],
    );

    // Rewrite `copy` (conflicting tip) and add another local commit on
    // `to-rebase`, then rebase onto the rewritten copy via a named remote so
    // remote-tracking + fork-point match t5520's `me` setup.
    git(&repo, &["remote", "add", "-f", "me", "."]);
    git(&repo, &["checkout", "copy", "-q"]);
    git(&repo, &["tag", "copy-orig"]);
    git(&repo, &["reset", "--hard", "HEAD^", "-q"]);
    fs::write(repo.join("file"), b"conflicting modification\n").expect("write");
    git_with_identity(&repo, &["commit", "-a", "-m", "conflict", "-q"]);
    git(&repo, &["checkout", "to-rebase", "-q"]);
    fs::write(repo.join("file2"), b"file\n").expect("write");
    git_with_identity(&repo, &["commit", "-a", "-m", "to-rebase", "-q"]);
    git(&repo, &["tag", "to-rebase-orig"]);
    run_success(
        sley_testkit::oracle_git(),
        &repo,
        &["pull", "--rebase", "me", "copy"],
    );

    git(
        &repo,
        &["checkout", "-b", "copy2", "to-rebase-orig", "-q"],
    );
    // The cell under test: pull --rebase against a *local* branch (remote `.`).
    let output = run_output_with_identity(
        sley_testkit::sley_bin!(),
        &repo,
        &["pull", "--rebase", ".", "to-rebase"],
    );
    assert!(
        output.status.success(),
        "pull --rebase against local branch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(repo.join("file")).expect("read file"),
        "conflicting modification\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("file2")).expect("read file2"),
        "file\n"
    );
    let _ = fs::remove_dir_all(&root);
}
