//! Differential interop tests for `git apply`, `git gc`, `git maintenance run`,
//! and `git repack` against the system `git` binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

#[test]
fn apply_modifies_worktree_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-mod");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "a\nb\nc\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    write_file(&repo, "f.txt", "a\nB\nc\n");
    let patch = git(&repo, &["diff"]).stdout;
    git_ok(&repo, &["checkout", "--", "f.txt"]);
    let patch_path = root.join("change.patch");
    fs::write(&patch_path, &patch).expect("write patch");
    let patch_arg = patch_path.to_str().expect("test operation should succeed");

    let candidate = root.join("candidate");
    let reference = root.join("reference");
    copy_dir_all(&repo, &candidate);
    copy_dir_all(&repo, &reference);

    let rs = sley(&candidate, &["apply", patch_arg]);
    assert!(
        rs.status.success(),
        "sley apply failed: {}",
        String::from_utf8_lossy(&rs.stderr)
    );
    git_ok(&reference, &["apply", patch_arg]);
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        fs::read(reference.join("f.txt")).expect("test operation should succeed"),
        "sley apply produced different bytes than git apply"
    );
    assert_eq!(
        fs::read(candidate.join("f.txt")).expect("test operation should succeed"),
        b"a\nB\nc\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_check_succeeds_for_clean_patch() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-check");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "1\n2\n3\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    write_file(&repo, "f.txt", "1\n2\nthree\n");
    let patch = git(&repo, &["diff"]).stdout;
    git_ok(&repo, &["checkout", "--", "f.txt"]);
    let patch_path = root.join("c.patch");
    fs::write(&patch_path, &patch).expect("write patch");

    // --check must not modify the worktree and must succeed for an applicable patch.
    let out = sley(
        &repo,
        &[
            "apply",
            "--check",
            patch_path.to_str().expect("test operation should succeed"),
        ],
    );
    assert!(
        out.status.success(),
        "sley apply --check rejected a clean patch: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(repo.join("f.txt")).expect("test operation should succeed"),
        b"1\n2\n3\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_plain_patch_outside_repository_matches_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-outside-repository");
    let reference = root.join("reference");
    let candidate = root.join("candidate");
    fs::create_dir_all(&reference).expect("create reference directory");
    fs::create_dir_all(&candidate).expect("create candidate directory");
    write_file(&reference, "nums", "one\ntwo\nthree\nfour\n");
    write_file(&candidate, "nums", "one\ntwo\nthree\nfour\n");
    let patch = root.join("change.patch");
    fs::write(
        &patch,
        b"diff --git a/nums b/nums\n--- a/nums\n+++ b/nums\n@@ -2,3 +2,4 @@ one\n two\n three\n four\n+five\n",
    )
    .expect("write patch");
    let patch = patch.to_str().expect("utf8 patch path");

    let expected = git(&reference, &["apply", patch]);
    let actual = sley(&candidate, &["apply", patch]);
    assert_eq!(actual.status.code(), expected.status.code());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    assert_eq!(
        fs::read(candidate.join("nums")).expect("read candidate"),
        fs::read(reference.join("nums")).expect("read reference")
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn empty_imap_send_and_remote_http_usage_match_git_outside_repository() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("outside-repository-helper-errors");
    let global_config = root.join("global-config");
    fs::write(
        &global_config,
        b"[imap]\n\thost = imaps://localhost\n\tfolder = Drafts\n",
    )
    .expect("write isolated global config");
    let run = |program: &str, args: &[&str]| {
        Command::new(program)
            .current_dir(&root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &global_config)
            .output()
            .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
    };
    for args in [&["imap-send", "-v"][..], &["remote-http"][..]] {
        let expected = run(sley_testkit::oracle_git(), args);
        let actual = run(sley_testkit::sley_bin!(), args);
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
        assert_eq!(actual.stdout, expected.stdout, "{args:?}");
        assert_eq!(actual.stderr, expected.stderr, "{args:?}");
    }
    fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_creates_new_file_like_git() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("apply-new");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "seed.txt", "seed\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    // Stage a new file, capture its creation patch, then unstage/remove it.
    write_file(&repo, "added.txt", "hello\nworld\n");
    git_ok(&repo, &["add", "added.txt"]);
    let patch = git(&repo, &["diff", "--cached"]).stdout;
    git_ok(&repo, &["rm", "-f", "--quiet", "added.txt"]);
    let patch_path = root.join("new.patch");
    fs::write(&patch_path, &patch).expect("write patch");

    let out = sley(
        &repo,
        &[
            "apply",
            patch_path.to_str().expect("test operation should succeed"),
        ],
    );
    assert!(
        out.status.success(),
        "sley apply (new file) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(repo.join("added.txt")).expect("test operation should succeed"),
        b"hello\nworld\n"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn gc_consolidates_loose_objects_and_stays_valid() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("gc");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    for i in 0..3 {
        write_file(&repo, "f.txt", &format!("v{i}\n"));
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-qm", &format!("c{i}")]);
    }
    let head_before = git(&repo, &["rev-parse", "HEAD"]).stdout;

    let out = sley(&repo, &["gc"]);
    assert!(
        out.status.success(),
        "sley gc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A pack now exists.
    let pack_dir = repo.join(".git/objects/pack");
    let has_pack = fs::read_dir(&pack_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "pack").unwrap_or(false))
        })
        .unwrap_or(false);
    assert!(has_pack, "sley gc did not produce a pack");

    // Repo is still valid and complete according to upstream git.
    let fsck = git(&repo, &["fsck", "--no-progress"]);
    assert!(
        fsck.status.success(),
        "git fsck failed after sley gc: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]).stdout, head_before);
    let log_lines = String::from_utf8_lossy(&git(&repo, &["log", "--oneline"]).stdout)
        .lines()
        .count();
    assert_eq!(log_lines, 3, "history not fully readable after gc");

    fs::remove_dir_all(&root).ok();
}

fn pack_dir_has_pack(repo: &Path) -> bool {
    fs::read_dir(repo.join(".git/objects/pack"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().map(|x| x == "pack").unwrap_or(false))
        })
        .unwrap_or(false)
}

#[test]
fn maintenance_run_matches_git_gc_behavior() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-run");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    for i in 0..3 {
        write_file(&repo, "f.txt", &format!("v{i}\n"));
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-qm", &format!("c{i}")]);
    }
    let head_before = git(&repo, &["rev-parse", "HEAD"]).stdout;

    let candidate = root.join("candidate");
    let reference = root.join("reference");
    copy_dir_all(&repo, &candidate);
    copy_dir_all(&repo, &reference);

    let rs = sley(&candidate, &["maintenance", "run"]);
    assert!(
        rs.status.success(),
        "sley maintenance run failed: {}",
        String::from_utf8_lossy(&rs.stderr)
    );
    git_ok(&reference, &["maintenance", "run"]);

    assert!(
        pack_dir_has_pack(&candidate),
        "sley maintenance run did not produce a pack"
    );
    assert!(
        pack_dir_has_pack(&reference),
        "git maintenance run did not produce a pack"
    );

    for (label, path) in [("sley", &candidate), ("git", &reference)] {
        let fsck = git(path, &["fsck", "--no-progress"]);
        assert!(
            fsck.status.success(),
            "git fsck failed after {label} maintenance run: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );
        assert_eq!(
            git(path, &["rev-parse", "HEAD"]).stdout,
            head_before,
            "{label} maintenance run changed HEAD"
        );
        let log_lines = String::from_utf8_lossy(&git(path, &["log", "--oneline"]).stdout)
            .lines()
            .count();
        assert_eq!(
            log_lines, 3,
            "history not fully readable after {label} maintenance run"
        );
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_reflog_expire_auto_counts_head_only() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-reflog-head-only");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "base\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);
    git_ok(&repo, &["branch", "noisy"]);

    let head = String::from_utf8(git(&repo, &["rev-parse", "HEAD"]).stdout)
        .expect("head oid utf8")
        .trim()
        .to_string();
    let zero = "0".repeat(head.len());
    let branch_log = repo.join(".git/logs/refs/heads/noisy");
    let mut log = String::new();
    for i in 0..120 {
        log.push_str(&format!(
            "{zero} {head} Tester <tester@example.com> 1 +0000\tbranch: noisy {i}\n"
        ));
    }
    fs::write(&branch_log, log).expect("write noisy branch reflog");

    let needed = sley(
        &repo,
        &["maintenance", "is-needed", "--auto", "--task=reflog-expire"],
    );
    assert!(
        !needed.status.success(),
        "non-HEAD reflogs should not trip reflog-expire auto"
    );

    let run = sley(
        &repo,
        &["maintenance", "run", "--auto", "--task=reflog-expire"],
    );
    assert!(
        run.status.success(),
        "maintenance run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let retained = fs::read_to_string(&branch_log)
        .expect("read branch reflog")
        .lines()
        .count();
    assert_eq!(retained, 120, "auto maintenance expired a non-HEAD reflog");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_run_aborts_when_lock_exists() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-lock");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    fs::write(repo.join(".git/objects/maintenance.lock"), b"in use\n").expect("write lock");

    let out = sley(&repo, &["maintenance", "run"]);
    assert!(
        !out.status.success(),
        "maintenance run unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("'maintenance' lock held by another process"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_run_quiet_accepted() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-quiet");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    write_file(&repo, "f.txt", "hello\n");
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-qm", "base"]);

    let out = sley(&repo, &["maintenance", "run", "--quiet"]);
    assert!(
        out.status.success(),
        "sley maintenance run --quiet failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_start_resolves_the_platform_scheduler_before_registering() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-start-auto-scheduler");
    let repo = root.join("repo");
    let global_config = root.join("global-config");
    git_ok(
        &root,
        &["init", "-q", repo.to_str().expect("utf8 repository path")],
    );

    let out = Command::new(sley_testkit::sley_bin!())
        .current_dir(&repo)
        .args(["maintenance", "start"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env(
            "GIT_TEST_MAINT_SCHEDULER",
            "crontab:true,systemctl:true,launchctl:true,schtasks:true",
        )
        .output()
        .expect("run maintenance start");
    assert!(
        out.status.success(),
        "automatic scheduler resolution failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let global = fs::read_to_string(&global_config).expect("read global config");
    assert!(global.contains("[maintenance]"), "{global}");
    let registered_repo = fs::canonicalize(&repo).expect("canonical repository path");
    assert!(
        global.contains(&format!("repo = {}", registered_repo.display())),
        "{global}"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn maintenance_start_does_not_register_when_auto_scheduler_is_unavailable() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("maint-start-unavailable-scheduler");
    let repo = root.join("repo");
    let global_config = root.join("global-config");
    git_ok(
        &root,
        &["init", "-q", repo.to_str().expect("utf8 repository path")],
    );

    let out = Command::new(sley_testkit::sley_bin!())
        .current_dir(&repo)
        .args(["maintenance", "start"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .env("HOME", &root)
        .env(
            "GIT_TEST_MAINT_SCHEDULER",
            "crontab:false,systemctl:false,launchctl:false,schtasks:false",
        )
        .output()
        .expect("run maintenance start");
    assert!(!out.status.success(), "unavailable scheduler succeeded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("scheduler is not available"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !global_config.exists(),
        "failed scheduling registered the repository"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn repack_d_keeps_repository_complete() {
    if !git_available() {
        return;
    }
    let root = unique_temp_dir("repack");
    let repo = root.join("repo");
    git_ok(
        &root,
        &[
            "init",
            "-q",
            repo.to_str().expect("test operation should succeed"),
        ],
    );
    for i in 0..4 {
        write_file(&repo, "f.txt", &format!("line {i}\ncommon\n"));
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-qm", &format!("c{i}")]);
    }
    let out = sley(&repo, &["repack", "-d"]);
    assert!(
        out.status.success(),
        "sley repack -d failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fsck = git(&repo, &["fsck", "--no-progress"]);
    assert!(
        fsck.status.success(),
        "git fsck failed after sley repack -d: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    assert!(
        git(&repo, &["cat-file", "-e", "HEAD^{tree}"])
            .status
            .success()
    );

    fs::remove_dir_all(&root).ok();
}
