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

#[test]
fn config_get_set_add_and_unset_match_upstream_git() {
    let root = unique_temp_dir("config");
    let upstream = root.join("upstream");
    let rust = root.join("rust");
    fs::create_dir_all(&upstream).expect("create upstream repo");
    fs::create_dir_all(&rust).expect("create rust repo");
    let result = (|| {
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

        for (key, value) in [
            ("pack.windowmemory", "2k"),
            ("pack.bigfilethreshold", "3m"),
            ("pack.depth", "-5"),
        ] {
            let args = ["config", key, value];
            assert_eq!(git(&upstream, &args), git_rs(&rust, &args));
        }
        assert_outputs_match(&upstream, &rust, &["config", "--int", "pack.windowmemory"]);
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
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
