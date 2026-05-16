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

fn run(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
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

fn prepare_remote_repo(root: &Path) -> PathBuf {
    let remote = root.join("remote");
    fs::create_dir_all(&remote).expect("create remote");
    run_success("git", &remote, &["init", "-q"]);
    run_success(
        "git",
        &remote,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    run_success("git", &remote, &["branch", "feature/topic"]);
    run_success("git", &remote, &["branch", "alpha"]);
    run_success("git", &remote, &["branch", "zed"]);
    run_success("git", &remote, &["tag", "light"]);
    run_success("git", &remote, &["tag", "v2"]);
    run_success("git", &remote, &["tag", "v10"]);
    run_success(
        "git",
        &remote,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "tag",
            "-a",
            "ann",
            "-m",
            "annotated",
        ],
    );
    remote
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

#[test]
fn ls_remote_local_repository_matches_upstream_git() {
    let root = unique_temp_dir("ls-remote-local");
    let result = (|| {
        fs::create_dir_all(&root).expect("create root");
        let remote = prepare_remote_repo(&root);
        let remote_arg = remote.to_string_lossy().to_string();
        let remote_file_url = file_url(&remote);
        let root_file_url = file_url(&root);

        for args in [
            vec!["ls-remote", remote_arg.as_str()],
            vec!["ls-remote", "--heads", remote_arg.as_str()],
            vec!["ls-remote", "--branches", remote_arg.as_str()],
            vec!["ls-remote", "-b", remote_arg.as_str()],
            vec!["ls-remote", "--tags", remote_arg.as_str()],
            vec!["ls-remote", "-t", remote_arg.as_str()],
            vec!["ls-remote", "--refs", remote_arg.as_str()],
            vec!["ls-remote", "--sort=refname", remote_arg.as_str()],
            vec!["ls-remote", "--sort=-refname", remote_arg.as_str()],
            vec![
                "ls-remote",
                "--tags",
                "--sort=version:refname",
                remote_arg.as_str(),
            ],
            vec![
                "ls-remote",
                "--tags",
                "--sort=-version:refname",
                remote_arg.as_str(),
            ],
            vec![
                "ls-remote",
                "--sort=refname",
                "--no-sort",
                remote_arg.as_str(),
            ],
            vec!["ls-remote", "--sort=objectname", remote_arg.as_str()],
            vec!["ls-remote", "--sort=objecttype", remote_arg.as_str()],
            vec!["ls-remote", "--sort=objectsize", remote_arg.as_str()],
            vec!["ls-remote", "--sort=objectsize:disk", remote_arg.as_str()],
            vec!["ls-remote", "--sort=authordate", remote_arg.as_str()],
            vec!["ls-remote", "--sort=committerdate", remote_arg.as_str()],
            vec!["ls-remote", "--sort=taggerdate", remote_arg.as_str()],
            vec!["ls-remote", "--sort=creatordate", remote_arg.as_str()],
            vec!["ls-remote", "--symref", remote_arg.as_str(), "HEAD"],
            vec![
                "ls-remote",
                remote_arg.as_str(),
                "feature/topic",
                "ann",
                "refs/heads/main",
            ],
            vec!["ls-remote", remote_arg.as_str(), "l*"],
            vec!["ls-remote", "--exit-code", remote_arg.as_str(), "missing"],
            vec!["ls-remote", remote_arg.as_str(), "missing"],
            vec!["ls-remote", "--get-url", remote_arg.as_str()],
            vec!["ls-remote", "--sort=bad", remote_arg.as_str()],
            vec!["ls-remote", remote_file_url.as_str()],
            vec!["ls-remote", "--heads", remote_file_url.as_str()],
            vec!["ls-remote", "--tags", remote_file_url.as_str()],
            vec!["ls-remote", "--refs", remote_file_url.as_str()],
            vec!["ls-remote", "--symref", remote_file_url.as_str(), "HEAD"],
            vec![
                "ls-remote",
                remote_file_url.as_str(),
                "feature/topic",
                "ann",
            ],
            vec!["ls-remote", "--get-url", remote_file_url.as_str()],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &root, &args);
            assert_same_output(actual, expected, &args);
        }

        for args in [
            vec!["ls-remote", "--sort=objecttype", "."],
            vec!["ls-remote", "--sort=-objecttype", "."],
            vec!["ls-remote", "--sort=objectsize", "."],
            vec!["ls-remote", "--sort=-objectsize", "."],
            vec!["ls-remote", "--sort=objectsize:disk", "."],
            vec!["ls-remote", "--sort=-objectsize:disk", "."],
            vec!["ls-remote", "--sort=authordate", "."],
            vec!["ls-remote", "--sort=-authordate", "."],
            vec!["ls-remote", "--sort=committerdate", "."],
            vec!["ls-remote", "--sort=-committerdate", "."],
            vec!["ls-remote", "--sort=taggerdate", "."],
            vec!["ls-remote", "--sort=-taggerdate", "."],
            vec!["ls-remote", "--sort=creatordate", "."],
            vec!["ls-remote", "--sort=-creatordate", "."],
            vec!["ls-remote", "--sort=objectname", remote_file_url.as_str()],
            vec!["ls-remote", "--sort=objecttype", remote_file_url.as_str()],
            vec!["ls-remote", "--sort=objectsize", remote_file_url.as_str()],
            vec![
                "ls-remote",
                "--sort=objectsize:disk",
                remote_file_url.as_str(),
            ],
            vec!["ls-remote", "--sort=authordate", remote_file_url.as_str()],
            vec![
                "ls-remote",
                "--sort=committerdate",
                remote_file_url.as_str(),
            ],
            vec!["ls-remote", "--sort=taggerdate", remote_file_url.as_str()],
            vec!["ls-remote", "--sort=creatordate", remote_file_url.as_str()],
        ] {
            let expected = run("git", &remote, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &remote, &args);
            assert_same_output(actual, expected, &args);
        }

        let client = root.join("client");
        run_success("git", &root, &["init", "-q", "client"]);
        run_success(
            "git",
            &client,
            &[
                "config",
                "url.file-alias/.insteadOf",
                root.to_string_lossy().as_ref(),
            ],
        );
        run_success(
            "git",
            &client,
            &[
                "config",
                format!("url.{root_file_url}/.insteadOf").as_str(),
                "alias/",
            ],
        );

        for args in [
            vec!["ls-remote", "--get-url", remote_arg.as_str()],
            vec!["ls-remote", "--get-url", "alias/remote"],
            vec!["ls-remote", "alias/remote"],
            vec!["ls-remote", "--heads", "alias/remote"],
        ] {
            let expected = run("git", &client, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &client, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn ls_remote_configured_local_remote_matches_upstream_git() {
    let root = unique_temp_dir("ls-remote-configured");
    let result = (|| {
        fs::create_dir_all(&root).expect("create root");
        prepare_remote_repo(&root);
        let remote_file_url = file_url(&root.join("remote"));
        let root_file_url = file_url(&root);
        let client = root.join("client");
        run_success("git", &root, &["init", "-q", "client"]);
        run_success("git", &client, &["remote", "add", "origin", "../remote"]);
        run_success(
            "git",
            &client,
            &["remote", "add", "file-origin", remote_file_url.as_str()],
        );
        run_success(
            "git",
            &client,
            &[
                "config",
                format!("url.{root_file_url}/.insteadOf").as_str(),
                "alias/",
            ],
        );
        run_success(
            "git",
            &client,
            &["remote", "add", "alias-origin", "alias/remote"],
        );

        for args in [
            vec!["ls-remote", "origin"],
            vec!["ls-remote", "--heads", "origin"],
            vec!["ls-remote", "--tags", "origin"],
            vec!["ls-remote", "--sort=refname", "origin"],
            vec!["ls-remote", "--sort", "-refname", "origin"],
            vec!["ls-remote", "--sort=objectname", "origin"],
            vec!["ls-remote", "--sort=-objectname", "origin"],
            vec!["ls-remote", "--sort=objecttype", "origin"],
            vec!["ls-remote", "--sort=objectsize", "origin"],
            vec!["ls-remote", "--sort=objectsize:disk", "origin"],
            vec!["ls-remote", "--sort=authordate", "origin"],
            vec!["ls-remote", "--sort=committerdate", "origin"],
            vec!["ls-remote", "--sort=taggerdate", "origin"],
            vec!["ls-remote", "--sort=creatordate", "origin"],
            vec!["ls-remote", "--get-url", "origin"],
            vec!["ls-remote", "--exit-code", "origin", "missing"],
            vec!["ls-remote", "file-origin"],
            vec!["ls-remote", "--heads", "file-origin"],
            vec!["ls-remote", "--tags", "file-origin"],
            vec!["ls-remote", "--sort=objectname", "file-origin"],
            vec!["ls-remote", "--get-url", "file-origin"],
            vec!["ls-remote", "alias-origin"],
            vec!["ls-remote", "--heads", "alias-origin"],
            vec!["ls-remote", "--get-url", "alias-origin"],
        ] {
            let expected = run("git", &client, &args);
            let actual = run(env!("CARGO_BIN_EXE_git-rs"), &client, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
