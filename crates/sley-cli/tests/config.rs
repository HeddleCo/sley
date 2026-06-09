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
    std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
}

fn run(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
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

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_input(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(stdin)
        .unwrap_or_else(|err| panic!("failed to write stdin for {program} {args:?}: {err}"));
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn git(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run("git", cwd, args)
}

fn git_rs(cwd: &Path, args: &[&str]) -> Vec<u8> {
    run(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn assert_outputs_match(upstream: &Path, rust: &Path, args: &[&str]) {
    let expected = git(upstream, args);
    let actual = git_rs(rust, args);
    assert_eq!(actual, expected, "sley output differed for {args:?}");
}

fn assert_status_stdout_stderr_match(upstream: &Path, rust: &Path, args: &[&str]) {
    let expected = run_output("git", upstream, args);
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

fn assert_status_match(upstream: &Path, rust: &Path, args: &[&str]) {
    let expected = run_output("git", upstream, args);
    let actual = run_output(env!("CARGO_BIN_EXE_sley"), rust, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "sley status differed for {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
}

fn assert_status_stdout_stderr_match_with_input(
    upstream: &Path,
    rust: &Path,
    args: &[&str],
    stdin: &[u8],
) {
    let expected = run_output_with_input("git", upstream, args, stdin);
    let actual = run_output_with_input(env!("CARGO_BIN_EXE_sley"), rust, args, stdin);
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
fn config_modern_subcommands_match_upstream_git_for_local_config() {
    let root = unique_temp_dir("config-subcommands");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);

        let set_name = ["config", "set", "--local", "user.name", "Ada Lovelace"];
        assert_eq!(git(&upstream, &set_name), git_rs(&rust, &set_name));
        assert_outputs_match(&upstream, &rust, &["config", "get", "--local", "user.name"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "get",
                "--local",
                "--show-origin",
                "--show-scope",
                "user.name",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "get",
                "-z",
                "--local",
                "--show-origin",
                "--show-scope",
                "user.name",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "-z", "--local", "user.name"],
        );
        assert_outputs_match(&upstream, &rust, &["config", "list", "--local"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "list", "--local", "--show-origin", "--show-scope"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "list", "--local", "--name-only"],
        );

        let set_bool = ["config", "set", "--local", "feature.enabled", "yes"];
        assert_eq!(git(&upstream, &set_bool), git_rs(&rust, &set_bool));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--local", "--bool", "feature.enabled"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "get",
                "--local",
                "--default=fallback",
                "feature.missing",
            ],
        );

        let append_main = [
            "config",
            "set",
            "--local",
            "--append",
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        ];
        assert_eq!(git(&upstream, &append_main), git_rs(&rust, &append_main));
        let append_dev = [
            "config",
            "set",
            "--local",
            "--append",
            "remote.origin.fetch",
            "+refs/heads/dev:refs/remotes/origin/dev",
        ];
        assert_eq!(git(&upstream, &append_dev), git_rs(&rust, &append_dev));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--local", "--all", "remote.origin.fetch"],
        );

        assert_status_match(
            &upstream,
            &rust,
            &["config", "unset", "--local", "remote.origin.fetch"],
        );
        let unset_all = ["config", "unset", "--local", "--all", "remote.origin.fetch"];
        assert_eq!(git(&upstream, &unset_all), git_rs(&rust, &unset_all));
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "get", "--local", "--all", "remote.origin.fetch"],
        );

        let unset_name = ["config", "unset", "--local", "user.name"];
        assert_eq!(git(&upstream, &unset_name), git_rs(&rust, &unset_name));
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "get", "--local", "user.name"],
        );

        for (key, value) in [("demo.old.one", "1"), ("demo.old.two", "2")] {
            let args = ["config", "set", "--local", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        let rename_section = [
            "config",
            "rename-section",
            "--local",
            "demo.old",
            "demo.new",
        ];
        assert_eq!(
            git(&upstream, &rename_section),
            git_rs(&rust, &rename_section)
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--local", "demo.new.one"],
        );
        assert_status_match(
            &upstream,
            &rust,
            &[
                "config",
                "rename-section",
                "--local",
                "missing.section",
                "renamed.section",
            ],
        );
        let remove_section = ["config", "remove-section", "--local", "demo.new"];
        assert_eq!(
            git(&upstream, &remove_section),
            git_rs(&rust, &remove_section)
        );
        assert_status_match(
            &upstream,
            &rust,
            &["config", "remove-section", "--local", "demo.new"],
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_file_and_stdin_sources_match_upstream_git() {
    let root = unique_temp_dir("config-file-source");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        let upstream_file = upstream.join("custom.config");
        let rust_file = rust.join("custom.config");
        fs::write(&upstream_file, b"[core]\n\teditor = vim\n").expect("write upstream config");
        fs::write(&rust_file, b"[core]\n\teditor = vim\n").expect("write rust config");

        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--file", "custom.config", "core.editor"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "list", "--file", "custom.config"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "get",
                "--file",
                "custom.config",
                "--show-origin",
                "--show-scope",
                "core.editor",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "list",
                "--file",
                "custom.config",
                "--show-origin",
                "--show-scope",
            ],
        );

        let set_args = [
            "config",
            "set",
            "--file",
            "custom.config",
            "core.pager",
            "less",
        ];
        assert_eq!(git(&upstream, &set_args), git_rs(&rust, &set_args));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--file", "custom.config", "core.pager"],
        );

        let unset_args = ["config", "unset", "--file", "custom.config", "core.editor"];
        assert_eq!(git(&upstream, &unset_args), git_rs(&rust, &unset_args));
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "get", "--file", "custom.config", "core.editor"],
        );

        let create_args = [
            "config",
            "set",
            "--file",
            "created.config",
            "core.editor",
            "nano",
        ];
        assert_eq!(git(&upstream, &create_args), git_rs(&rust, &create_args));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "get", "--file", "created.config", "core.editor"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "get", "--file", "missing.config", "core.editor"],
        );
        assert_status_match(
            &upstream,
            &rust,
            &["config", "list", "--file", "missing.config"],
        );

        let stdin_config = b"[core]\n\teditor = emacs\n";
        assert_status_stdout_stderr_match_with_input(
            &upstream,
            &rust,
            &["config", "get", "--file", "-", "core.editor"],
            stdin_config,
        );
        assert_status_stdout_stderr_match_with_input(
            &upstream,
            &rust,
            &["config", "list", "--file", "-"],
            stdin_config,
        );
        assert_status_stdout_stderr_match_with_input(
            &upstream,
            &rust,
            &[
                "config",
                "get",
                "--file",
                "-",
                "--show-origin",
                "--show-scope",
                "core.editor",
            ],
            stdin_config,
        );
        assert_status_stdout_stderr_match_with_input(
            &upstream,
            &rust,
            &[
                "config",
                "list",
                "-z",
                "--file",
                "-",
                "--show-origin",
                "--show-scope",
            ],
            stdin_config,
        );
        assert_status_match(
            &upstream,
            &rust,
            &["config", "set", "--file", "-", "core.editor", "ed"],
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_writes_preserve_git_case_rules() {
    let root = unique_temp_dir("config-case");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        fs::write(upstream.join("custom.config"), b"[Core]\n\tEditor = vim\n")
            .expect("write upstream config");
        fs::write(rust.join("custom.config"), b"[Core]\n\tEditor = vim\n")
            .expect("write rust config");

        for args in [
            [
                "config",
                "set",
                "--file",
                "custom.config",
                "core.editor",
                "nano",
            ],
            [
                "config",
                "set",
                "--file",
                "custom.config",
                "Core.Pager",
                "less",
            ],
            [
                "config",
                "rename-section",
                "--file",
                "custom.config",
                "Core",
                "Settings.Main",
            ],
        ] {
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_eq!(
            fs::read(upstream.join("custom.config")).expect("read upstream config"),
            fs::read(rust.join("custom.config")).expect("read rust config"),
            "sley config writer case differed from upstream git"
        );

        let create_args = [
            "config",
            "set",
            "--file",
            "created.config",
            "Camel.Section.Mixed-Key",
            "Value",
        ];
        assert_eq!(git(&upstream, &create_args), git_rs(&rust, &create_args));
        assert_eq!(
            fs::read(upstream.join("created.config")).expect("read upstream created config"),
            fs::read(rust.join("created.config")).expect("read rust created config"),
            "sley config writer case differed when creating a new file"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_comment_writes_match_upstream_git() {
    let root = unique_temp_dir("config-comments");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        fs::write(upstream.join("custom.config"), b"").expect("write upstream config");
        fs::write(rust.join("custom.config"), b"").expect("write rust config");

        for args in [
            [
                "config",
                "set",
                "--file",
                "custom.config",
                "--comment",
                "hello world",
                "user.name",
                "Ada",
            ],
            [
                "config",
                "--file",
                "custom.config",
                "--add",
                "--comment",
                "# alias",
                "user.alias",
                "A",
            ],
            [
                "config",
                "--file",
                "custom.config",
                "--replace-all",
                "--comment",
                "second",
                "user.name",
                "Grace",
            ],
        ] {
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_eq!(
            fs::read(upstream.join("custom.config")).expect("read upstream config"),
            fs::read(rust.join("custom.config")).expect("read rust config"),
            "sley config --comment writer differed from upstream git"
        );

        assert_status_match(
            &upstream,
            &rust,
            &[
                "config",
                "--file",
                "custom.config",
                "--get",
                "--comment",
                "nope",
                "user.name",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "set",
                "--file",
                "custom.config",
                "--comment",
                "bad\ncomment",
                "user.email",
                "ada@example.invalid",
            ],
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_rejects_incompatible_mode_combinations_like_upstream_git() {
    let root = unique_temp_dir("config-mode-combo");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    assert_status_stdout_stderr_match(&upstream, &rust, &["config", "--get", "--get-all"]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_trailing_cr_round_trip_matches_upstream_git() {
    let root = unique_temp_dir("config-trailing-cr");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);
        let value = format!("bar{}", '\r');
        let set_args = ["config", "set", "core.foo", value.as_str()];
        assert_eq!(git(&upstream, &set_args), git_rs(&rust, &set_args));
        assert_outputs_match(&upstream, &rust, &["config", "get", "core.foo"]);
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_invalid_keys_and_implicit_bool_get_match_upstream_git() {
    let root = unique_temp_dir("config-edge-cases");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream dir");
    fs::create_dir_all(&rust).expect("create rust dir");
    {
        let config_path = "bare.config";
        let config_bytes = b"[core]\n\tflag\n\tempty =\n";
        fs::write(upstream.join(config_path), config_bytes).expect("write upstream config");
        fs::write(rust.join(config_path), config_bytes).expect("write rust config");

        for args in [
            ["config", "get", "invalid"],
            ["config", "get", "core."],
            ["config", "get", "core.0b"],
        ] {
            assert_status_stdout_stderr_match(&upstream, &rust, &args);
        }

        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--file", config_path, "--get", "core.flag"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--file",
                config_path,
                "--bool",
                "--get",
                "core.flag",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--file", config_path, "--get", "core.empty"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--file",
                config_path,
                "--get-regexp",
                "--bool",
                "^core",
            ],
        );

        let bad_config = b"[core]\n\tx = bad\n[broken\n";
        fs::write(upstream.join("bad.config"), bad_config).expect("write upstream bad config");
        fs::write(rust.join("bad.config"), bad_config).expect("write rust bad config");
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--file", "bad.config", "--list"],
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn config_get_set_add_and_unset_match_upstream_git() {
    let root = unique_temp_dir("config");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    {
        git(&upstream, &["init", "-q"]);
        git(&rust, &["init", "-q"]);

        let set_name = ["config", "user.name", "Ada Lovelace"];
        assert_eq!(git(&upstream, &set_name), git_rs(&rust, &set_name));
        assert_outputs_match(&upstream, &rust, &["config", "user.name"]);
        assert_outputs_match(&upstream, &rust, &["config", "--get", "user.name"]);
        assert_eq!(
            git(&rust, &["config", "--get", "user.name"]),
            b"Ada Lovelace\n"
        );

        let set_email = ["config", "--local", "user.email", "ada@example.invalid"];
        assert_eq!(git(&upstream, &set_email), git_rs(&rust, &set_email));
        assert_outputs_match(&upstream, &rust, &["config", "--get", "user.email"]);

        let add_main = [
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/main:refs/remotes/origin/main",
        ];
        assert_eq!(git(&upstream, &add_main), git_rs(&rust, &add_main));
        let add_dev = [
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/dev:refs/remotes/origin/dev",
        ];
        assert_eq!(git(&upstream, &add_dev), git_rs(&rust, &add_dev));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        assert_eq!(
            git(&rust, &["config", "--get-all", "remote.origin.fetch"]),
            git(&upstream, &["config", "--get-all", "remote.origin.fetch"])
        );
        assert_status_match(
            &upstream,
            &rust,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/replaced:refs/remotes/origin/replaced",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        let replace_all = [
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/replaced:refs/remotes/origin/replaced",
        ];
        assert_eq!(git(&upstream, &replace_all), git_rs(&rust, &replace_all));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        let add_dev = [
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/dev:refs/remotes/origin/dev",
        ];
        assert_eq!(git(&upstream, &add_dev), git_rs(&rust, &add_dev));
        let replace_main_only = [
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/trunk:refs/remotes/origin/trunk",
            "replaced",
        ];
        assert_eq!(
            git(&upstream, &replace_main_only),
            git_rs(&rust, &replace_main_only)
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        let append_no_match = [
            "config",
            "--replace-all",
            "remote.origin.fetch",
            "+refs/heads/topic:refs/remotes/origin/topic",
            "missing",
        ];
        assert_eq!(
            git(&upstream, &append_no_match),
            git_rs(&rust, &append_no_match)
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        let unset_dev_only = ["config", "--unset", "remote.origin.fetch", "dev"];
        assert_eq!(
            git(&upstream, &unset_dev_only),
            git_rs(&rust, &unset_dev_only)
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-all", "remote.origin.fetch"],
        );
        assert_status_match(
            &upstream,
            &rust,
            &["config", "--unset-all", "remote.origin.fetch", "missing"],
        );
        for (key, value) in [("demo.old.one", "1"), ("demo.old.two", "2")] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        let rename_section = ["config", "--rename-section", "demo.old", "demo.new"];
        assert_eq!(
            git(&upstream, &rename_section),
            git_rs(&rust, &rename_section)
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get", "demo.new.one"]);
        assert_status_match(
            &upstream,
            &rust,
            &["config", "--remove-section", "missing.section"],
        );
        let remove_section = ["config", "--remove-section", "demo.new"];
        assert_eq!(
            git(&upstream, &remove_section),
            git_rs(&rust, &remove_section)
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--local", "--get-regexp", "^demo\\."],
        );
        assert_outputs_match(&upstream, &rust, &["config", "--local", "--list"]);
        assert_outputs_match(&upstream, &rust, &["config", "--local", "-l"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--local", "--list", "--name-only"],
        );
        assert_outputs_match(&upstream, &rust, &["config", "-z", "--get", "user.name"]);
        assert_outputs_match(&upstream, &rust, &["config", "-z", "--local", "--list"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--local", "--list", "--name-only"],
        );

        for (key, value) in [
            ("feature.enabled", "yes"),
            ("feature.disabled", "off"),
            ("feature.numeric", "1"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(&upstream, &rust, &["config", "--bool", "feature.enabled"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get", "--bool", "feature.disabled"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--type=bool", "--get", "feature.numeric"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--default",
                "fallback",
                "--get",
                "feature.missing",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--bool",
                "--default=yes",
                "--get",
                "feature.missing",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--int",
                "--default",
                "4k",
                "--get",
                "feature.missing",
            ],
        );
        assert_status_stdout_stderr_match(&upstream, &rust, &["config", "--default"]);
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--default=fallback",
                "--get-all",
                "feature.missing",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--default=fallback", "--list"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--file",
                ".git/config",
                "--default=fallback",
                "get",
                "feature.missing",
            ],
        );
        let add_bool = ["config", "--add", "feature.multi", "true"];
        assert_eq!(git(&upstream, &add_bool), git_rs(&rust, &add_bool));
        let add_bool = ["config", "--add", "feature.multi", "no"];
        assert_eq!(git(&upstream, &add_bool), git_rs(&rust, &add_bool));
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--bool", "--get-all", "feature.multi"],
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-regexp", "^feature\\."]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--get-regexp", "^feature\\."],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--name-only", "--get-regexp", "^feature\\."],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--name-only", "--get-regexp", "^feature\\."],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--bool", "--get-regexp", "^feature\\.multi$"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-regexp", "^missing\\."],
        );
        assert_status_match(&upstream, &rust, &["config", "--unset", "feature.multi"]);
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "feature.multi"]);
        let unset_multi = ["config", "--unset-all", "feature.multi"];
        assert_eq!(git(&upstream, &unset_multi), git_rs(&rust, &unset_multi));
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-all", "feature.multi"],
        );

        for value in ["one", "tone", "two"] {
            let args = ["config", "--add", "pattern.multi", value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_status_match(
            &upstream,
            &rust,
            &["config", "--unset", "pattern.multi", "o.*e"],
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "pattern.multi"]);
        let replace_many = ["config", "--replace-all", "pattern.multi", "X", "o.*e"];
        assert_eq!(git(&upstream, &replace_many), git_rs(&rust, &replace_many));
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "pattern.multi"]);
        let unset_all_pattern = ["config", "--unset-all", "pattern.multi", "t.*"];
        assert_eq!(
            git(&upstream, &unset_all_pattern),
            git_rs(&rust, &unset_all_pattern)
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "pattern.multi"]);
        for value in ["o.*e", "one"] {
            let args = ["config", "--add", "fixed.multi", value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        let unset_fixed = ["config", "--unset", "--fixed-value", "fixed.multi", "o.*e"];
        assert_eq!(git(&upstream, &unset_fixed), git_rs(&rust, &unset_fixed));
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "fixed.multi"]);
        let replace_fixed_missing = [
            "config",
            "--replace-all",
            "--fixed-value",
            "fixed.multi",
            "literal",
            "missing",
        ];
        assert_eq!(
            git(&upstream, &replace_fixed_missing),
            git_rs(&rust, &replace_fixed_missing)
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "fixed.multi"]);
        let replace_fixed = [
            "config",
            "--replace-all",
            "--fixed-value",
            "fixed.multi",
            "exact",
            "literal",
        ];
        assert_eq!(
            git(&upstream, &replace_fixed),
            git_rs(&rust, &replace_fixed)
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-all", "fixed.multi"]);

        for (key, value) in [
            ("pack.windowmemory", "2k"),
            ("pack.bigfilethreshold", "3m"),
            ("pack.depth", "-5"),
            ("pack.hex", "0x10"),
            ("pack.octal", "010"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(&upstream, &rust, &["config", "--int", "pack.windowmemory"]);
        assert_outputs_match(&upstream, &rust, &["config", "--int", "pack.hex"]);
        assert_outputs_match(&upstream, &rust, &["config", "--int", "pack.octal"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--get-all", "remote.origin.fetch"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get", "--type=int", "pack.bigfilethreshold"],
        );
        assert_outputs_match(&upstream, &rust, &["config", "--int", "pack.depth"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--bool-or-int", "feature.enabled"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--bool-or-int", "feature.disabled"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--bool-or-int", "pack.windowmemory"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--type=bool-or-int", "pack.hex"],
        );

        for (key, value) in [
            ("gc.pruneexpire", "1700000000"),
            ("gc.prunenow", "now"),
            ("gc.prunenever", "never"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--expiry-date", "gc.pruneexpire"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--type=expiry-date", "gc.prunenow"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--expiry-date", "gc.prunenever"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--expiry-date",
                "--default=never",
                "--get",
                "gc.missing",
            ],
        );

        for (key, value) in [
            ("color.simple", "red"),
            ("color.attr", "bold red"),
            ("color.fgbg", "red blue"),
            ("color.rgb", "#112233"),
            ("color.indexed", "12"),
            ("color.normal", "normal"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
            assert_outputs_match(&upstream, &rust, &["config", "--type=color", key]);
        }
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--type=color",
                "--default=red",
                "--get",
                "color.missing",
            ],
        );
        assert_outputs_match(&upstream, &rust, &["config", "--get-color", "color.attr"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-color", "color.missing", "blue"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--get-color", "color.attr"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-color", "color.missing"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--show-origin", "--get-color", "color.attr"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-color", "color.missing", "badcolor"],
        );

        for (key, value) in [
            ("color.always", "always"),
            ("color.boolean", "true"),
            ("color.auto", "auto"),
            ("color.invalid", "maybe"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.always", "true"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "-z", "--get-colorbool", "color.always", "true"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.boolean", "true"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.auto", "true"],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.missing", "true"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.always"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.boolean"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.missing"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.invalid"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--get-colorbool", "color.always", "bad"],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--show-origin",
                "--get-colorbool",
                "color.always",
                "true",
            ],
        );

        for (key, value) in [
            ("http.sslVerify", "true"),
            ("http.https://example.com/.sslVerify", "false"),
            ("http.https://EXAMPLE.com:443/repo/.sslVerify", "canonical"),
            ("http.https://user@example.com/repo/.sslVerify", "user"),
            ("http.https://[2001:db8::1]/repo/.sslVerify", "ipv6"),
            ("http.https://[2001:db8::1]:444/repo/.extra", "ported"),
            ("http.https://example.com/repo/.extra", "one"),
            ("http.https://example.com/a%20b/.postBuffer", "encoded"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http",
                "https://example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "HTTPS://example.COM/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://example.com:443/repo",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://example.com:444/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://user@example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://other@example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://[2001:DB8::1]:443/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.extra",
                "https://[2001:db8::1]:444/repo/path",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.extra",
                "https://[2001:db8::1]/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "-z",
                "--get-urlmatch",
                "http",
                "https://example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.sslVerify",
                "https://example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.extra",
                "https://example.com/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.postBuffer",
                "https://example.com/a b/file",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.postBuffer",
                "https://example.com/a%2fb/file",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http.extra",
                "https://example.org/repo/path",
            ],
        );
        assert_outputs_match(
            &upstream,
            &rust,
            &[
                "config",
                "--get-urlmatch",
                "http",
                "https://example.org/repo/path",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--name-only",
                "--get-urlmatch",
                "http",
                "https://example.com/repo/path",
            ],
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &[
                "config",
                "--show-origin",
                "--get-urlmatch",
                "http",
                "https://example.com/repo/path",
            ],
        );

        let path_args = ["config", "core.editorpath", "~/bin/editor"];
        assert_eq!(git(&upstream, &path_args), git_rs(&rust, &path_args));
        assert_outputs_match(&upstream, &rust, &["config", "--path", "core.editorpath"]);
        assert_outputs_match(
            &upstream,
            &rust,
            &["config", "--type=path", "core.editorpath"],
        );

        let unset_email = ["config", "--unset", "user.email"];
        assert_eq!(
            git(&upstream, &unset_email),
            git_rs(&rust, &unset_email),
            "sley unset output differed"
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--local", "--get", "user.email"],
        );

        let unset_fetch = ["config", "--unset-all", "remote.origin.fetch"];
        assert_eq!(
            git(&upstream, &unset_fetch),
            git_rs(&rust, &unset_fetch),
            "sley unset-all output differed"
        );
        assert_status_stdout_stderr_match(
            &upstream,
            &rust,
            &["config", "--local", "--get-all", "remote.origin.fetch"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}
