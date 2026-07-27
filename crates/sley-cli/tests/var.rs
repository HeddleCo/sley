use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const VAR_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Author One"),
    ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
    ("GIT_AUTHOR_DATE", "@1234567890 -0500"),
    ("GIT_COMMITTER_NAME", "Committer One"),
    ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
    ("GIT_COMMITTER_DATE", "@1234567891 +0230"),
    ("GIT_EDITOR", "nano -w"),
    ("GIT_PAGER", "less -R"),
];

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn init_repo(name: &str) -> PathBuf {
    let root = unique_temp_dir(name);
    std::fs::create_dir_all(&root).expect("create temp dir");
    let status = Command::new(sley_testkit::oracle_git())
        .arg("init")
        .arg(&root)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");
    root
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    let home = cwd.join("home");
    let xdg = cwd.join("xdg");
    std::fs::create_dir_all(&home).expect("create isolated home");
    std::fs::create_dir_all(&xdg).expect("create isolated xdg");
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .env_remove("GIT_SEQUENCE_EDITOR")
        .env_remove("GIT_CONFIG_COUNT")
        .envs(VAR_ENV.iter().copied())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn assert_status_stdout_stderr_match(cwd: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), cwd, args);
    let actual = run_output(sley_testkit::sley_bin!(), cwd, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "sley stdout differed for {args:?}"
    );
    assert_eq!(
        actual.stderr, expected.stderr,
        "sley stderr differed for {args:?}"
    );
}

#[test]
fn var_named_values_match_upstream_git() {
    let repo = init_repo("var-named");
    {
        for args in [
            vec!["var", "GIT_AUTHOR_IDENT"],
            vec!["var", "GIT_COMMITTER_IDENT"],
            vec!["var", "GIT_EDITOR"],
            vec!["var", "GIT_SEQUENCE_EDITOR"],
            vec!["var", "GIT_PAGER"],
            vec!["var", "GIT_DEFAULT_BRANCH"],
            vec!["var", "GIT_SHELL_PATH"],
            vec!["-c", "core.editor=vim -n", "var", "GIT_EDITOR"],
            vec!["-c", "sequence.editor=sed -i", "var", "GIT_SEQUENCE_EDITOR"],
            vec![
                "-c",
                "init.defaultBranch=trunk",
                "var",
                "GIT_DEFAULT_BRANCH",
            ],
            vec!["var"],
            vec!["var", "UNKNOWN"],
            vec!["var", "-x"],
            vec!["var", "-l", "extra"],
        ] {
            assert_status_stdout_stderr_match(&repo, &args);
        }
    }
    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn var_list_reports_config_overrides_and_computed_values() {
    let repo = init_repo("var-list");
    let status = Command::new(sley_testkit::oracle_git())
        .current_dir(&repo)
        .args(["config", "init.defaultBranch", "localmain"])
        .status()
        .expect("run git config");
    assert!(status.success(), "git config failed");

    let actual = run_output(
        sley_testkit::sley_bin!(),
        &repo,
        &["-c", "init.defaultBranch=trunk", "var", "-l"],
    );
    assert!(actual.status.success(), "sley var -l failed");
    let stdout = String::from_utf8(actual.stdout).expect("utf8 stdout");
    for expected in [
        "core.repositoryformatversion=0\n",
        "init.defaultbranch=localmain\n",
        "init.defaultbranch=trunk\n",
        "GIT_COMMITTER_IDENT=Committer One <committer@example.invalid> 1234567891 +0230\n",
        "GIT_AUTHOR_IDENT=Author One <author@example.invalid> 1234567890 -0500\n",
        "GIT_EDITOR=nano -w\n",
        "GIT_SEQUENCE_EDITOR=nano -w\n",
        "GIT_PAGER=less -R\n",
        "GIT_DEFAULT_BRANCH=trunk\n",
        "GIT_SHELL_PATH=/bin/sh\n",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in var -l output:\n{stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// t7005-editor #1: with no GIT_EDITOR/VISUAL/EDITOR/core.editor and a non-dumb
/// TERM, `git var GIT_EDITOR` falls back to the compiled default (`vi`).
#[test]
fn var_git_editor_default_matches_upstream_git() {
    let repo = init_repo("var-editor-default");
    {
        let home = repo.join("home");
        let xdg = repo.join("xdg");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&xdg).expect("xdg");

        for (term, expect_success) in [("vt100", true), ("dumb", false)] {
            let run = |program: &str| {
                Command::new(program)
                    .current_dir(&repo)
                    .args(["var", "GIT_EDITOR"])
                    .env("HOME", &home)
                    .env("XDG_CONFIG_HOME", &xdg)
                    .env("TERM", term)
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env_remove("GIT_EDITOR")
                    .env_remove("VISUAL")
                    .env_remove("EDITOR")
                    .env_remove("GIT_SEQUENCE_EDITOR")
                    .output()
                    .unwrap_or_else(|err| panic!("failed to run {program}: {err}"))
            };
            let expected = run(sley_testkit::oracle_git());
            let actual = run(sley_testkit::sley_bin!());
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "status for TERM={term}"
            );
            assert_eq!(actual.stdout, expected.stdout, "stdout for TERM={term}");
            if expect_success {
                assert!(actual.status.success(), "TERM={term} should succeed");
                let editor = String::from_utf8_lossy(&actual.stdout);
                assert!(
                    !editor.trim().is_empty(),
                    "default editor must be non-empty for TERM={term}, got {editor:?}"
                );
            } else {
                assert!(
                    !actual.status.success(),
                    "TERM=dumb should fail without EDITOR"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&repo);
}
