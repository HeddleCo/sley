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

fn assert_status_stdout_stderr_match(upstream: &Path, rust: &Path, args: &[&str]) {
    let expected = run_output(sley_testkit::oracle_git(), upstream, args);
    let actual = run_output(env!("CARGO_BIN_EXE_sley"), rust, args);
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
fn alias_init_via_dash_c_matches_upstream_git_outside_repo() {
    let root = unique_temp_dir("alias-init-dash-c");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        let args = ["-c", "alias.aliasedinit=init", "aliasedinit", "-q"];
        assert_status_stdout_stderr_match(&upstream, &rust, &args);
        assert!(upstream.join(".git").is_dir(), "upstream should init .git");
        assert!(rust.join(".git").is_dir(), "sley should init .git");
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn alias_init_via_config_matches_upstream_git_outside_repo() {
    let root = unique_temp_dir("alias-init-config");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    let upstream_config = root.join("upstream.gitconfig");
    let rust_config = root.join("rust.gitconfig");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        for (dir, config_path, program) in [
            (&upstream, &upstream_config, sley_testkit::oracle_git()),
            (&rust, &rust_config, env!("CARGO_BIN_EXE_sley")),
        ] {
            let output = Command::new(program)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", config_path)
                .env_remove("GIT_CONFIG_SYSTEM")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("HOME")
                .args([
                    "config",
                    "--file",
                    config_path.to_str().expect("utf8 path"),
                    "alias.aliasedinit",
                    "init",
                ])
                .output()
                .unwrap_or_else(|err| panic!("failed to run {program} config: {err}"));
            assert!(
                output.status.success(),
                "{program} config alias failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        for (dir, config_path) in [(&upstream, &upstream_config), (&rust, &rust_config)] {
            let expected = Command::new(sley_testkit::oracle_git())
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", config_path)
                .env_remove("GIT_CONFIG_SYSTEM")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("HOME")
                .args(["aliasedinit", "-q"])
                .output()
                .expect("upstream git aliasedinit");
            let actual = Command::new(env!("CARGO_BIN_EXE_sley"))
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", config_path)
                .env_remove("GIT_CONFIG_SYSTEM")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("HOME")
                .args(["aliasedinit", "-q"])
                .output()
                .expect("sley aliasedinit");
            assert_eq!(actual.status.code(), expected.status.code());
            assert_eq!(actual.stdout, expected.stdout);
            assert_eq!(actual.stderr, expected.stderr);
            assert!(dir.join(".git").is_dir());
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn alias_nested_expansion_matches_upstream_git() {
    let root = unique_temp_dir("alias-nested");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        let args = [
            "-c",
            "alias.outer=aliasedinit",
            "-c",
            "alias.aliasedinit=init",
            "outer",
            "-q",
        ];
        assert_status_stdout_stderr_match(&upstream, &rust, &args);
        assert!(upstream.join(".git").is_dir());
        assert!(rust.join(".git").is_dir());
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn alias_dash_c_version_aliases_match_upstream_git() {
    let root = unique_temp_dir("alias-config-cmds");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        for (alias, expansion) in [("hello-world", "version"), ("CamelCase", "version")] {
            let args = ["-c", &format!("alias.{alias}={expansion}"), alias];
            assert_status_stdout_stderr_match(&upstream, &rust, &args);
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn alias_checkconfig_expands_instead_of_unsupported_command() {
    let root = unique_temp_dir("alias-checkconfig");
    fs::create_dir_all(&root).expect("create temp dir");
    {
        let output = run_output(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["-c", "alias.checkconfig=config --list", "checkconfig"],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("unsupported command checkconfig"),
            "alias should expand checkconfig to config, got stderr: {stderr}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn builtin_init_is_not_replaced_by_alias() {
    let root = unique_temp_dir("alias-builtin-init");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        // Built-in `init` must not be expanded even when alias.init is set.
        let args = ["-c", "alias.init=version", "init", "-q"];
        assert_status_stdout_stderr_match(&upstream, &rust, &args);
        assert!(upstream.join(".git").is_dir());
        assert!(rust.join(".git").is_dir());
    }
    let _ = fs::remove_dir_all(&root);
}
