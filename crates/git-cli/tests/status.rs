use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
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

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_git-rs"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

#[test]
fn status_z_matches_upstream_git() {
    let root = unique_temp_dir("status-z");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("hello.txt"), b"hello\n").expect("write staged fixture");
        fs::write(root.join("extra.txt"), b"extra\n").expect("write untracked fixture");
        fs::create_dir_all(root.join("dir/sub")).expect("create untracked directory fixture");
        fs::write(root.join("dir/a.txt"), b"a\n").expect("write untracked directory file");
        fs::write(root.join("dir/sub/b.txt"), b"b\n").expect("write nested untracked file");
        fs::create_dir_all(root.join("empty-dir")).expect("create empty directory fixture");
        fs::create_dir_all(root.join("tracked-dir")).expect("create tracked directory fixture");
        fs::write(root.join("tracked-dir/base.txt"), b"base\n").expect("write tracked dir fixture");
        fs::write(root.join("tracked-dir/extra.txt"), b"extra\n")
            .expect("write untracked file under tracked dir");
        git(&root, &["add", "hello.txt", "tracked-dir/base.txt"]);

        for args in [
            vec!["status", "-s", "-z"],
            vec!["status", "--short", "-z"],
            vec!["status", "--short", "--branch"],
            vec!["status", "-s", "-b"],
            vec!["status", "-b", "-s"],
            vec!["status", "-sb"],
            vec!["status", "-bs"],
            vec!["status", "--short", "--no-short"],
            vec!["status", "--no-short", "--short"],
            vec!["status", "--short", "--branch", "--no-branch"],
            vec!["status", "-sb", "--no-branch"],
            vec!["status", "--short", "--no-branch", "--branch"],
            vec!["status", "--short", "--branch", "-z"],
            vec!["status", "-sb", "-z"],
            vec!["status", "-bs", "--null"],
            vec!["status", "--short", "--branch", "--null"],
            vec!["status", "--short", "--null", "--no-null"],
            vec!["status", "--short", "-u"],
            vec!["status", "--short", "-uno"],
            vec!["status", "--short", "--untracked-files"],
            vec!["status", "--short", "--untracked-files=no"],
            vec!["status", "--short", "--untracked-files="],
            vec!["status", "--short", "--no-untracked-files"],
            vec![
                "status",
                "--short",
                "--untracked-files=no",
                "--untracked-files",
            ],
            vec![
                "status",
                "--short",
                "--no-untracked-files",
                "--untracked-files",
            ],
            vec![
                "status",
                "--short",
                "--untracked-files",
                "--untracked-files=no",
            ],
            vec![
                "status",
                "--short",
                "--untracked-files",
                "--no-untracked-files",
            ],
            vec!["status", "--short", "--untracked-files=no", "-z"],
            vec!["status", "--short", "--no-untracked-files", "-z"],
            vec!["status", "--short", "--branch", "--untracked-files=no"],
            vec!["status", "--short", "--branch", "--no-untracked-files"],
            vec!["status", "--short", "-unormal"],
            vec!["status", "--short", "-uall"],
            vec!["status", "--short", "--untracked-files=normal"],
            vec!["status", "--short", "--untracked-files=all"],
            vec!["status", "--short", "-uall", "-z"],
            vec!["status", "--porcelain"],
            vec!["status", "--porcelain=1"],
            vec!["status", "--porcelain", "-z"],
            vec!["status", "--porcelain", "--null"],
            vec!["status", "--porcelain", "--null", "--no-null"],
            vec!["status", "--porcelain", "--branch"],
            vec!["status", "--porcelain=1", "--branch"],
            vec!["status", "--porcelain", "--branch", "--no-branch"],
            vec!["status", "--porcelain", "--branch", "-z"],
            vec!["status", "--no-porcelain", "--short"],
            vec!["status", "--porcelain", "--no-porcelain", "--short"],
            vec!["status", "--porcelain", "--untracked-files"],
            vec!["status", "--porcelain", "--untracked-files=all"],
            vec!["status", "--porcelain", "--untracked-files=all", "-z"],
            vec!["status", "--porcelain", "--untracked-files=no"],
            vec!["status", "--porcelain", "--no-untracked-files"],
            vec!["status", "--porcelain=1", "--untracked-files=no"],
            vec!["status", "--porcelain=1", "--no-untracked-files"],
            vec!["status", "--porcelain=v1", "-z"],
            vec!["status", "--porcelain=v1", "--null"],
            vec!["status", "--porcelain=v1", "--untracked-files"],
            vec!["status", "--porcelain=v1", "--untracked-files=all"],
            vec!["status", "--porcelain=v1", "--branch"],
            vec!["status", "--porcelain=v1", "--branch", "-z"],
            vec!["status", "--porcelain=v2"],
            vec!["status", "--porcelain=2"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "-z"],
            vec!["status", "--porcelain=v2", "--branch", "--no-branch"],
            vec!["status", "--porcelain=v2", "-z"],
            vec!["status", "--porcelain=v2", "--untracked-files=no"],
            vec!["status", "--porcelain=v2", "--no-untracked-files"],
            vec!["status", "--porcelain=v2", "--untracked-files=all", "-z"],
            vec!["status", "-z"],
            vec!["status", "--null"],
            vec!["status", "--null", "--no-short"],
            vec!["status", "--no-short", "--null"],
            vec!["status", "--null", "--no-long"],
            vec!["status", "--no-long", "--null"],
            vec!["status", "--short", "--ignored=no"],
            vec!["status", "--short", "--ignored"],
            vec!["status", "--short", "--ignored=traditional"],
            vec!["status", "--short", "--ignored=matching"],
            vec!["status", "--short", "--no-ignored"],
            vec!["status", "--porcelain", "--ignored=no"],
            vec!["status", "--porcelain", "--ignored"],
            vec!["status", "--porcelain", "--ignored=traditional"],
            vec!["status", "--porcelain", "--ignored=matching"],
            vec!["status", "--porcelain=v1", "--no-ignored"],
            vec!["status", "--short", "--no-renames"],
            vec!["status", "--short", "--renames"],
            vec!["status", "--short", "-M"],
            vec!["status", "--short", "-M20%"],
            vec!["status", "--short", "--find-renames"],
            vec!["status", "--short", "--find-renames=20%"],
            vec!["status", "--short", "--find-renames=50%"],
            vec!["status", "--short", "--find-renames=abc"],
            vec!["status", "--porcelain", "--no-renames"],
            vec!["status", "--porcelain", "--renames"],
            vec!["status", "--porcelain", "-M"],
            vec!["status", "--porcelain", "-M100%"],
            vec!["status", "--porcelain", "--find-renames"],
            vec!["status", "--porcelain", "--find-renames=20%"],
            vec!["status", "--porcelain", "--find-renames=50%"],
            vec!["status", "--short", "--ahead-behind"],
            vec!["status", "--short", "--no-ahead-behind"],
            vec!["status", "--short", "-v"],
            vec!["status", "--short", "--verbose"],
            vec!["status", "--short", "--verbose", "--no-verbose"],
            vec!["status", "--porcelain", "--show-stash"],
            vec!["status", "--porcelain", "--no-show-stash"],
            vec!["status", "--porcelain", "--verbose"],
            vec!["status", "--porcelain", "--no-verbose"],
            vec!["status", "--short", "--column"],
            vec!["status", "--short", "--no-column"],
            vec!["status", "--short", "--column="],
            vec!["status", "--short", "--column=auto"],
            vec!["status", "--short", "--column=always"],
            vec!["status", "--short", "--column=never"],
            vec!["status", "--short", "--column=plain"],
            vec!["status", "--short", "--column=column"],
            vec!["status", "--short", "--column=row"],
            vec!["status", "--short", "--column=dense"],
            vec!["status", "--short", "--column=nodense"],
            vec!["status", "--porcelain", "--column=auto"],
            vec!["status", "--porcelain", "--column=never"],
            vec!["status", "--porcelain", "--column=plain"],
            vec!["status", "--short", "--ignore-submodules"],
            vec!["status", "--short", "--ignore-submodules=none"],
            vec!["status", "--short", "--ignore-submodules=untracked"],
            vec!["status", "--short", "--ignore-submodules=dirty"],
            vec!["status", "--short", "--ignore-submodules=all"],
            vec!["status", "--short", "--no-ignore-submodules"],
            vec!["status", "--porcelain", "--ignore-submodules"],
            vec!["status", "--porcelain", "--ignore-submodules=all"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_display_option_errors_match_upstream_git() {
    let root = unique_temp_dir("status-display-option-errors");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");

        for args in [
            vec!["status", "--short=value"],
            vec!["status", "--no-short=value"],
            vec!["status", "--branch=value"],
            vec!["status", "--no-branch=value"],
            vec!["status", "--null=value"],
            vec!["status", "--no-null=value"],
            vec!["status", "--long=value"],
            vec!["status", "--no-long=value"],
            vec!["status", "--ahead-behind=value"],
            vec!["status", "--no-ahead-behind=value"],
            vec!["status", "--verbose=value"],
            vec!["status", "--no-verbose=value"],
            vec!["status", "--show-stash=value"],
            vec!["status", "--no-show-stash=value"],
            vec!["status", "--porcelain=bad"],
            vec!["status", "--porcelain="],
            vec!["status", "--no-porcelain=value"],
            vec!["status", "--renames=value"],
            vec!["status", "--no-renames=value"],
            vec!["status", "--untracked-files=bad"],
            vec!["status", "-ubad"],
            vec!["status", "--ignored=bad"],
            vec!["status", "--ignored="],
            vec!["status", "--no-ignored=value"],
            vec!["status", "--ignore-submodules=bad"],
            vec!["status", "--ignore-submodules="],
            vec!["status", "--no-ignore-submodules=value"],
            vec!["status", "--column=bad"],
            vec!["status", "--no-column=value"],
            vec!["status", "--null", "--long"],
            vec!["status", "--long", "--null"],
        ] {
            let expected = run_output("git", &root, &args);
            let actual = run_output(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_porcelain_v2_tracked_changes_match_upstream_git() {
    let root = unique_temp_dir("status-v2-tracked");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("staged-delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("worktree-delete.txt"), b"delete\n").expect("write delete fixture");
        fs::write(root.join("staged-modify.txt"), b"base\n").expect("write modify fixture");
        fs::write(root.join("worktree-modify.txt"), b"base\n").expect("write modify fixture");
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("staged-add.txt"), b"add\n").expect("write add fixture");
        fs::write(root.join("staged-modify.txt"), b"staged\n").expect("write staged modify");
        fs::write(root.join("worktree-modify.txt"), b"worktree\n").expect("write worktree modify");
        fs::write(root.join("untracked.txt"), b"untracked\n").expect("write untracked fixture");
        fs::remove_file(root.join("worktree-delete.txt")).expect("remove worktree delete fixture");
        git(&root, &["add", "staged-add.txt", "staged-modify.txt"]);
        git(&root, &["rm", "-q", "staged-delete.txt"]);

        for args in [
            vec!["status", "--porcelain=v2"],
            vec!["status", "--porcelain=2"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "-z"],
            vec!["status", "--porcelain=v2", "--untracked-files=no"],
            vec![
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=no",
            ],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(actual, expected, "git-rs output differed for {args:?}");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_long_matches_upstream_git() {
    let root = unique_temp_dir("status-long");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(root.join("a.txt"), b"base\n").expect("write modify fixture");
        fs::write(root.join("d.txt"), b"delete\n").expect("write delete fixture");
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "base",
                "-q",
            ],
        );

        fs::write(root.join("a.txt"), b"staged\n").expect("write staged modify");
        fs::write(root.join("n.txt"), b"new\n").expect("write staged add");
        git(&root, &["add", "a.txt", "n.txt"]);
        fs::write(root.join("a.txt"), b"worktree\n").expect("write worktree modify");
        fs::remove_file(root.join("d.txt")).expect("remove tracked file");
        fs::write(root.join("u.txt"), b"untracked\n").expect("write untracked fixture");

        for args in [
            vec!["status"],
            vec!["status", "--long"],
            vec!["status", "--short", "--long"],
            vec!["status", "--long", "--short"],
            vec!["status", "--short", "--no-porcelain"],
            vec!["status", "--porcelain", "--no-porcelain"],
            vec!["status", "--porcelain=v2", "--no-porcelain"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs long status output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_long_unborn_clean_matches_upstream_git() {
    let root = unique_temp_dir("status-long-unborn-clean");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        for args in [
            vec!["status"],
            vec!["status", "--long"],
            vec!["status", "--short", "--long"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs unborn clean long status output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_show_stash_matches_upstream_git() {
    let root = unique_temp_dir("status-show-stash");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"base\n").expect("write tracked fixture");
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-m", "base", "-q"]);
        fs::write(root.join("a.txt"), b"stash one\n").expect("write first stash fixture");
        git(&root, &["stash", "push", "-q", "-m", "one"]);
        fs::write(root.join("a.txt"), b"stash two\n").expect("write second stash fixture");
        git(&root, &["stash", "push", "-q", "-m", "two"]);

        for args in [
            vec!["status", "--show-stash"],
            vec!["status", "--show-stash", "--no-show-stash"],
            vec!["status", "--no-show-stash", "--show-stash"],
            vec!["status", "--short", "--show-stash"],
            vec!["status", "--porcelain", "--show-stash"],
            vec!["status", "--show-stash", "--short"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs show-stash output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_branch_ahead_behind_matches_upstream_git() {
    let root = unique_temp_dir("status-branch-ahead-behind");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let peer = root.join("peer");
    fs::create_dir_all(&root).expect("create temp root");
    let remote_arg = remote.to_string_lossy().into_owned();
    let work_arg = work.to_string_lossy().into_owned();
    let peer_arg = peer.to_string_lossy().into_owned();
    let result = (|| {
        git(
            &root,
            &["init", "-q", "--bare", "--initial-branch=main", &remote_arg],
        );
        git(&root, &["clone", "-q", &remote_arg, &work_arg]);
        git(&work, &["config", "user.name", "Example User"]);
        git(&work, &["config", "user.email", "example@example.invalid"]);
        fs::write(work.join("a.txt"), b"base\n").expect("write base fixture");
        git(&work, &["add", "a.txt"]);
        git(&work, &["commit", "-m", "base", "-q"]);
        git(&work, &["push", "-q", "-u", "origin", "main"]);

        for args in [
            vec!["status"],
            vec!["status", "--no-ahead-behind"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--short", "--branch", "--no-ahead-behind"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "--no-ahead-behind"],
        ] {
            let expected = git(&work, &args);
            let actual = git_rs(&work, &args);
            assert_eq!(
                actual, expected,
                "git-rs clean tracking header differed for {args:?}"
            );
        }

        git(&root, &["clone", "-q", &remote_arg, &peer_arg]);
        git(&peer, &["config", "user.name", "Example User"]);
        git(&peer, &["config", "user.email", "example@example.invalid"]);
        fs::write(peer.join("b.txt"), b"remote\n").expect("write remote fixture");
        git(&peer, &["add", "b.txt"]);
        git(&peer, &["commit", "-m", "remote", "-q"]);
        git(&peer, &["push", "-q"]);
        git(&work, &["fetch", "-q"]);

        for args in [
            vec!["status"],
            vec!["status", "--no-ahead-behind"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--short", "--branch", "--no-ahead-behind"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "--no-ahead-behind"],
        ] {
            let expected = git(&work, &args);
            let actual = git_rs(&work, &args);
            assert_eq!(
                actual, expected,
                "git-rs behind tracking header differed for {args:?}"
            );
        }

        fs::write(work.join("a.txt"), b"local\n").expect("write local fixture");
        git(&work, &["commit", "-am", "local", "-q"]);
        for args in [
            vec!["status"],
            vec!["status", "--no-ahead-behind"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--short", "--branch", "--no-ahead-behind"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "--no-ahead-behind"],
        ] {
            let expected = git(&work, &args);
            let actual = git_rs(&work, &args);
            assert_eq!(
                actual, expected,
                "git-rs ahead tracking header differed for {args:?}"
            );
        }

        for args in [
            vec!["status"],
            vec!["status", "--no-ahead-behind"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--short", "--branch", "--no-ahead-behind"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "--no-ahead-behind"],
        ] {
            let expected = git(&work, &args);
            let actual = git_rs(&work, &args);
            assert_eq!(
                actual, expected,
                "git-rs divergent tracking header differed for {args:?}"
            );
        }

        git(&work, &["update-ref", "-d", "refs/remotes/origin/main"]);

        for args in [
            vec!["status"],
            vec!["status", "--no-ahead-behind"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--short", "--branch", "--no-ahead-behind"],
            vec!["status", "--porcelain=v2", "--branch"],
            vec!["status", "--porcelain=v2", "--branch", "--no-ahead-behind"],
        ] {
            let expected = git(&work, &args);
            let actual = git_rs(&work, &args);
            assert_eq!(
                actual, expected,
                "git-rs gone tracking header differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_detached_head_matches_upstream_git() {
    let root = unique_temp_dir("status-detached-head");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"base\n").expect("write base fixture");
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-m", "base", "-q"]);
        let base = String::from_utf8(git(&root, &["rev-parse", "HEAD"]))
            .expect("rev-parse output is utf8");
        fs::write(root.join("a.txt"), b"second\n").expect("write second fixture");
        git(&root, &["commit", "-am", "second", "-q"]);
        git(&root, &["checkout", "-q", base.trim()]);

        for args in [
            vec!["status"],
            vec!["status", "--short", "--branch"],
            vec!["status", "--porcelain=v2", "--branch"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs detached status output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_hides_root_gitignore_matches_like_upstream_git() {
    let root = unique_temp_dir("status-gitignore");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::write(
            root.join(".gitignore"),
            b"\\#ignored.hash\n\\!ignored.bang\n\\*.literal\nliteral-\\?.tmp\nliteral-\\[ab\\].tmp\ntrailing.log   \nliteral-space\\ \nclass-[ab].tmp\nrange-[0-2].tmp\nnegclass-[!z].tmp\n*.log\n!important.log\nignored-dir/\n",
        )
        .expect("write gitignore");
        fs::write(
            root.join(".git/info/exclude"),
            b"*.cache\n!important.cache\ninfo-dir/\n",
        )
        .expect("write exclude");
        fs::write(
            root.join("global-excludes"),
            b"*.global\n!important.global\n",
        )
        .expect("write configured excludes");
        git(&root, &["config", "core.excludesFile", "global-excludes"]);
        fs::write(root.join("ignored.log"), b"ignored\n").expect("write ignored fixture");
        fs::write(root.join("ignored.cache"), b"ignored\n").expect("write info ignored fixture");
        fs::write(root.join("ignored.global"), b"ignored\n").expect("write global ignored fixture");
        fs::write(root.join("#ignored.hash"), b"ignored\n").expect("write escaped hash fixture");
        fs::write(root.join("!ignored.bang"), b"ignored\n").expect("write escaped bang fixture");
        fs::write(root.join("*.literal"), b"ignored\n").expect("write escaped star fixture");
        fs::write(root.join("wild.literal"), b"visible\n")
            .expect("write escaped star visible fixture");
        fs::write(root.join("literal-?.tmp"), b"ignored\n")
            .expect("write escaped question fixture");
        fs::write(root.join("literal-a.tmp"), b"visible\n")
            .expect("write escaped question visible fixture");
        fs::write(root.join("literal-[ab].tmp"), b"ignored\n")
            .expect("write escaped class fixture");
        fs::write(root.join("trailing.log"), b"ignored\n").expect("write trailing-space fixture");
        fs::write(root.join("literal-space "), b"ignored\n").expect("write literal-space fixture");
        fs::write(root.join("class-a.tmp"), b"ignored\n").expect("write class fixture");
        fs::write(root.join("class-c.tmp"), b"visible\n").expect("write class visible fixture");
        fs::write(root.join("range-1.tmp"), b"ignored\n").expect("write range fixture");
        fs::write(root.join("range-9.tmp"), b"visible\n").expect("write range visible fixture");
        fs::write(root.join("negclass-a.tmp"), b"ignored\n").expect("write negated class fixture");
        fs::write(root.join("negclass-z.tmp"), b"visible\n")
            .expect("write negated class visible fixture");
        fs::write(root.join("important.log"), b"visible\n").expect("write negated fixture");
        fs::write(root.join("important.cache"), b"visible\n").expect("write info negated fixture");
        fs::write(root.join("important.global"), b"visible\n")
            .expect("write global negated fixture");
        fs::write(root.join("visible.tmp"), b"visible\n").expect("write visible fixture");
        fs::create_dir_all(root.join("ignored-dir")).expect("create ignored dir");
        fs::write(root.join("ignored-dir/file.txt"), b"ignored\n").expect("write ignored dir file");
        fs::create_dir_all(root.join("info-dir")).expect("create info ignored dir");
        fs::write(root.join("info-dir/file.txt"), b"ignored\n").expect("write info ignored file");
        fs::create_dir_all(root.join("local")).expect("create local ignore dir");
        fs::write(
            root.join("local/.gitignore"),
            b"*.local\n!important.local\n",
        )
        .expect("write local gitignore");
        fs::write(root.join("local/hidden.local"), b"ignored\n").expect("write local ignored file");
        fs::write(root.join("local/important.local"), b"visible\n")
            .expect("write local negated file");
        fs::write(root.join("local/tracked.txt"), b"tracked\n").expect("write local tracked file");
        git(&root, &["add", "local/tracked.txt"]);

        for args in [
            vec!["status", "--short"],
            vec!["status", "--porcelain"],
            vec!["status", "--porcelain=v2"],
            vec!["status", "--short", "-z"],
            vec!["status", "--porcelain=v2", "-z"],
            vec!["status", "--short", "--ignored"],
            vec!["status", "--short", "--ignored=traditional"],
            vec!["status", "--short", "--ignored=matching"],
            vec!["status", "--short", "--ignored", "--no-ignored"],
            vec!["status", "--short", "--ignored", "--ignored=no"],
            vec!["status", "--porcelain", "--ignored"],
            vec!["status", "--porcelain", "--ignored", "-z"],
            vec!["status", "--porcelain=v2", "--ignored"],
            vec!["status", "--porcelain=v2", "--ignored", "-z"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs ignored-file output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("status-pathspecs");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::create_dir_all(root.join("dir")).expect("create dir fixture");
        fs::create_dir_all(root.join("other")).expect("create other fixture");
        fs::write(root.join("a.txt"), b"base\n").expect("write root fixture");
        fs::write(root.join("dir/b.txt"), b"base\n").expect("write dir fixture");
        fs::write(root.join("other/c.txt"), b"base\n").expect("write other fixture");
        git(&root, &["add", "a.txt", "dir/b.txt", "other/c.txt"]);
        fs::write(root.join("a.txt"), b"modified\n").expect("modify root fixture");
        fs::write(root.join("dir/b.txt"), b"modified\n").expect("modify dir fixture");
        fs::write(root.join("dir/u.txt"), b"untracked\n").expect("write dir untracked fixture");
        fs::write(root.join("v.txt"), b"untracked\n").expect("write root untracked fixture");

        for args in [
            vec!["status", "--short", "a.txt"],
            vec!["status", "--short", "dir"],
            vec!["status", "--short", "dir/"],
            vec!["status", "--short", "missing"],
            vec!["status", "--short", "--", "a.txt"],
            vec!["status", "--short", "--", "dir", "--ignored"],
            vec!["status", "--porcelain", "dir"],
            vec!["status", "--porcelain=v2", "dir"],
            vec!["status", "--short", "-z", "dir"],
            vec!["status", "--porcelain=v2", "-z", "dir"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_eq!(
                actual, expected,
                "git-rs pathspec output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_nested_cwd_paths_match_upstream_git() {
    let root = unique_temp_dir("status-nested-cwd");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        git(&root, &["init", "-q"]);
        fs::create_dir_all(root.join("dir")).expect("create dir fixture");
        fs::create_dir_all(root.join("other")).expect("create other fixture");
        fs::write(root.join("dir/a.txt"), b"base\n").expect("write dir fixture");
        fs::write(root.join("other/b.txt"), b"base\n").expect("write other fixture");
        git(&root, &["add", "dir/a.txt", "other/b.txt"]);
        fs::write(root.join("dir/a.txt"), b"modified\n").expect("modify dir fixture");
        fs::write(root.join("other/b.txt"), b"modified\n").expect("modify other fixture");
        fs::write(root.join("dir/u.txt"), b"untracked\n").expect("write dir untracked fixture");
        fs::write(root.join("root.txt"), b"untracked\n").expect("write root untracked fixture");

        let cwd = root.join("dir");
        for args in [
            vec!["status", "--short"],
            vec!["status", "--short", "."],
            vec!["status", "--short", "a.txt"],
            vec!["status", "--short", "../other"],
            vec!["status", "--porcelain"],
            vec!["status", "--porcelain=v2"],
            vec!["status", "--porcelain=v2", "."],
            vec!["status", "--short", "-z"],
            vec!["status", "--short", "-z", "../other"],
            vec!["status", "--porcelain=v2", "-z"],
        ] {
            let expected = git(&cwd, &args);
            let actual = git_rs(&cwd, &args);
            assert_eq!(
                actual, expected,
                "git-rs nested-cwd output differed for {args:?}"
            );
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn status_quoted_paths_match_upstream_git() {
    let root = unique_temp_dir("status-quoted-paths");
    fs::create_dir_all(&root).expect("create temp root");
    let result = (|| {
        for (case, path) in [
            ("space", "space name.txt"),
            ("quote", "quote\"name.txt"),
            ("tab", "tab\tname.txt"),
        ] {
            let repo = root.join(case);
            fs::create_dir_all(&repo).expect("create case repo");
            git(&repo, &["init", "-q"]);
            fs::write(repo.join(path), b"initial\n").expect("write untracked fixture");

            for args in [
                vec!["status", "--short"],
                vec!["status", "--porcelain"],
                vec!["status", "--porcelain=v2"],
                vec!["status", "--short", "-z"],
                vec!["status", "--porcelain=v2", "-z"],
            ] {
                let expected = git(&repo, &args);
                let actual = git_rs(&repo, &args);
                assert_eq!(
                    actual, expected,
                    "git-rs untracked output differed for {args:?} path {path:?}"
                );
            }

            git(&repo, &["add", path]);
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "-m",
                    "base",
                    "-q",
                ],
            );
            fs::write(repo.join(path), b"modified\n").expect("modify tracked fixture");

            for args in [
                vec!["status", "--short"],
                vec!["status", "--porcelain"],
                vec!["status", "--porcelain=v2"],
                vec!["status", "--short", "-z"],
                vec!["status", "--porcelain=v2", "-z"],
            ] {
                let expected = git(&repo, &args);
                let actual = git_rs(&repo, &args);
                assert_eq!(
                    actual, expected,
                    "git-rs tracked output differed for {args:?} path {path:?}"
                );
            }
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
