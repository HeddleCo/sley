use std::fs;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
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

fn run_with_env(program: &str, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Vec<u8> {
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command
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

fn sley(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::sley_bin!(), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(sley_testkit::oracle_git(), cwd, args)
}

#[test]
#[cfg(unix)]
fn archive_tar_matches_upstream_git_for_commit_tree() {
    let root = unique_temp_dir("archive-tar");
    fs::create_dir_all(root.join("dir")).expect("create fixture dirs");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"hello\n").expect("write file");
        fs::write(root.join("dir").join("b.txt"), b"sub\n").expect("write nested file");
        fs::write(root.join("run"), b"#!/bin/sh\n").expect("write executable");
        let mut permissions = fs::metadata(root.join("run"))
            .expect("stat executable")
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        fs::set_permissions(root.join("run"), permissions).expect("chmod executable");
        unix_fs::symlink("a.txt", root.join("link")).expect("create symlink");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let expected = git(&root, &["archive", "--format=tar", "--prefix=pfx/", "HEAD"]);
        let actual = sley(&root, &["archive", "--format=tar", "--prefix=pfx/", "HEAD"]);
        assert_eq!(actual, expected);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn archive_output_option_writes_tar_file() {
    let root = unique_temp_dir("archive-output");
    fs::create_dir_all(&root).expect("create fixture dir");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"hello\n").expect("write file");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let archive = root.join("out.tar");
        sley(
            &root,
            &[
                "archive",
                "--format=tar",
                "-o",
                archive.to_str().expect("archive path is utf8"),
                "HEAD",
            ],
        );
        let actual = fs::read(&archive).expect("read sley archive");
        let expected = git(&root, &["archive", "--format=tar", "HEAD"]);
        assert_eq!(actual, expected);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn archive_tar_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("archive-pathspecs");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    fs::create_dir_all(root.join("other")).expect("create other dir");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"root\n").expect("write root file");
        fs::write(root.join("dir").join("b.txt"), b"sub\n").expect("write nested file");
        fs::write(root.join("dir/sub").join("c.txt"), b"deep\n").expect("write deep file");
        fs::write(root.join("other").join("o.txt"), b"other\n").expect("write other file");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        for args in [
            vec!["archive", "--format=tar", "HEAD", "dir/b.txt"],
            vec!["archive", "--format=tar", "HEAD", "dir"],
            vec!["archive", "--format=tar", "HEAD", "dir/"],
            vec!["archive", "--format=tar", "HEAD", "dir/b.txt", "dir"],
            vec![
                "archive",
                "--format=tar",
                "--prefix=pfx",
                "HEAD",
                "dir/b.txt",
            ],
            vec![
                "archive",
                "--format=tar",
                "--prefix=pfx/",
                "HEAD",
                "dir/b.txt",
            ],
            vec!["archive", "--format=tar", "HEAD", "a.txt", "other/o.txt"],
        ] {
            let expected = git(&root, &args);
            let actual = sley(&root, &args);
            assert_eq!(actual, expected, "archive output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn archive_tar_cwd_relative_pathspecs_match_upstream_git() {
    let root = unique_temp_dir("archive-cwd-pathspecs");
    fs::create_dir_all(root.join("dir/sub")).expect("create fixture dirs");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("root.txt"), b"root\n").expect("write root file");
        fs::write(root.join("dir").join("a.txt"), b"a\n").expect("write dir file");
        fs::write(root.join("dir/sub").join("b.txt"), b"b\n").expect("write nested file");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let cwd = root.join("dir");
        for args in [
            vec!["archive", "--format=tar", "HEAD"],
            vec!["archive", "--format=tar", "HEAD", "a.txt"],
            vec!["archive", "--format=tar", "HEAD", "sub"],
            vec![
                "archive",
                "--format=tar",
                "--prefix=pfx/",
                "HEAD",
                "sub/b.txt",
            ],
        ] {
            let expected = git(&cwd, &args);
            let actual = sley(&cwd, &args);
            assert_eq!(actual, expected, "archive output differed for {args:?}");
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn archive_missing_pathspec_errors_match_upstream_git() {
    let root = unique_temp_dir("archive-missing-pathspec");
    fs::create_dir_all(&root).expect("create fixture dir");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join("a.txt"), b"root\n").expect("write root file");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let args = ["archive", "--format=tar", "HEAD", "missing"];
        let expected = run_output(sley_testkit::oracle_git(), &root, &args);
        let actual = run_output(sley_testkit::sley_bin!(), &root, &args);
        assert_eq!(actual.status.code(), expected.status.code());
        assert!(
            actual.stdout.is_empty(),
            "stdout should be empty on failure"
        );
        assert!(
            String::from_utf8_lossy(&actual.stderr).contains("pathspec 'missing'"),
            "stderr should mention missing pathspec, got {}",
            String::from_utf8_lossy(&actual.stderr)
        );
    };
    let _ = fs::remove_dir_all(&root);
}

/// `git archive` runs `convert_to_working_tree` on every regular-file blob, so
/// with `core.autocrlf=true` the LF-normalized committed blob is smudged back to
/// CRLF in the archive (upstream t0024-crlf-archive). The archived bytes must
/// match oracle git byte-for-byte, including that symlinks are left unconverted.
#[test]
#[cfg(unix)]
fn archive_tar_applies_autocrlf_smudge_like_upstream() {
    let root = unique_temp_dir("archive-autocrlf");
    fs::create_dir_all(&root).expect("create fixture dir");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        git(&root, &["config", "core.autocrlf", "true"]);
        // Commit CRLF content; the clean filter normalizes it to LF in the blob,
        // and archive smudges it back to CRLF.
        fs::write(root.join("sample"), b"CRLF line ending\r\nAnd another\r\n")
            .expect("write crlf file");
        // A blob git treats as binary (lone CR / NUL) must pass through unchanged.
        fs::write(root.join("binary"), b"\x00\x01\r\x02").expect("write binary file");
        unix_fs::symlink("sample", root.join("link")).expect("create symlink");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let expected = git(&root, &["archive", "--format=tar", "HEAD"]);
        let actual = sley(&root, &["archive", "--format=tar", "HEAD"]);
        assert_eq!(actual, expected);
    };
    let _ = fs::remove_dir_all(&root);
}

/// Conversion attributes for `git archive` are read from the *archived tree*'s
/// `.gitattributes` (upstream sets `GIT_ATTR_INDEX`), so an `eol=crlf` rule
/// committed into the tree drives the smudge even with default config. The
/// archived bytes must match oracle git byte-for-byte.
#[test]
fn archive_tar_applies_gitattributes_eol_from_tree() {
    let root = unique_temp_dir("archive-attr-eol");
    fs::create_dir_all(&root).expect("create fixture dir");
    {
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Example User"]);
        git(&root, &["config", "user.email", "example@example.invalid"]);
        fs::write(root.join(".gitattributes"), b"*.txt text eol=crlf\n")
            .expect("write gitattributes");
        fs::write(root.join("a.txt"), b"one\ntwo\nthree\n").expect("write text file");
        fs::write(root.join("raw.bin"), b"one\ntwo\n").expect("write unattributed file");
        git(&root, &["add", "."]);
        run_with_env(
            sley_testkit::oracle_git(),
            &root,
            &["commit", "-m", "initial", "-q"],
            &[
                ("GIT_AUTHOR_DATE", "1700000000 +0000"),
                ("GIT_COMMITTER_DATE", "1700000000 +0000"),
            ],
        );

        let expected = git(&root, &["archive", "--format=tar", "HEAD"]);
        let actual = sley(&root, &["archive", "--format=tar", "HEAD"]);
        assert_eq!(actual, expected);
    };
    let _ = fs::remove_dir_all(&root);
}
