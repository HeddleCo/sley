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

fn read_head(root: &Path) -> Vec<u8> {
    fs::read(root.join(".git").join("HEAD")).expect("read HEAD")
}

fn ref_exists(root: &Path, name: &str) -> bool {
    root.join(".git").join(name).exists()
}

#[test]
fn symbolic_ref_quiet_matches_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-quiet");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success("git", &root, &["init", "-q"]);
        for args in [
            vec!["symbolic-ref", "--quiet", "HEAD"],
            vec!["symbolic-ref", "-q", "HEAD"],
            vec!["symbolic-ref", "--short", "--quiet", "HEAD"],
            vec!["symbolic-ref", "--short", "--", "HEAD"],
            vec!["symbolic-ref", "--quiet", "refs/heads/main"],
            vec!["symbolic-ref", "--quiet", "--no-quiet", "refs/heads/main"],
            vec!["symbolic-ref", "--no-quiet", "--quiet", "refs/heads/main"],
            vec!["symbolic-ref", "-q", "refs/heads/main"],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_reftable_repository_matches_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-reftable");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&root).expect("create temp root");
    {
        fs::create_dir_all(&expected).expect("create expected repo dir");
        fs::create_dir_all(&actual).expect("create actual repo dir");
        run_success("git", &expected, &["init", "-q", "--ref-format=reftable"]);
        run_success("git", &actual, &["init", "-q", "--ref-format=reftable"]);

        for args in [
            vec!["symbolic-ref", "refs/alias/rust", "refs/heads/main"],
            vec!["symbolic-ref", "refs/alias/rust"],
            vec!["symbolic-ref", "--delete", "refs/alias/rust"],
            vec!["symbolic-ref", "--quiet", "refs/alias/rust"],
        ] {
            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_update_options_match_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-update-options");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&root).expect("create temp root");
    {
        fs::create_dir_all(&expected).expect("create expected repo dir");
        fs::create_dir_all(&actual).expect("create actual repo dir");
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);

        for args in [
            vec!["symbolic-ref", "-m", "reason", "HEAD", "refs/heads/topic"],
            vec!["symbolic-ref", "-mreason", "HEAD", "refs/heads/topic"],
            vec!["symbolic-ref", "-m=reason", "HEAD", "refs/heads/topic"],
            vec!["symbolic-ref", "--", "HEAD", "refs/heads/topic"],
            vec!["symbolic-ref", "HEAD", "--", "refs/heads/topic"],
            vec![
                "symbolic-ref",
                "-m",
                "reason",
                "--short",
                "HEAD",
                "refs/heads/topic",
            ],
            vec![
                "symbolic-ref",
                "-m",
                "reason",
                "--quiet",
                "HEAD",
                "refs/heads/topic",
            ],
            vec!["symbolic-ref", "--short", "--quiet", "HEAD", "refs/tags/v1"],
            vec!["symbolic-ref", "HEAD", "refs/tags/v1"],
            vec!["symbolic-ref", "HEAD", "HEAD"],
            vec!["symbolic-ref", "-m"],
        ] {
            fs::write(
                expected.join(".git").join("HEAD"),
                b"ref: refs/heads/main\n",
            )
            .expect("reset expected HEAD");
            fs::write(actual.join(".git").join("HEAD"), b"ref: refs/heads/main\n")
                .expect("reset actual HEAD");

            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_eq!(
                read_head(&actual),
                read_head(&expected),
                "HEAD differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_delete_matches_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-delete");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);

        for args in [
            vec!["symbolic-ref", "--delete", "refs/alias/two"],
            vec!["symbolic-ref", "-d", "refs/alias/two"],
            vec!["symbolic-ref", "--delete", "--", "refs/alias/two"],
            vec!["symbolic-ref", "--delete", "-q", "refs/alias/two"],
            vec!["symbolic-ref", "--delete", "--no-delete", "refs/alias/two"],
            vec!["symbolic-ref", "--no-delete", "--delete", "refs/alias/two"],
            vec!["symbolic-ref", "--delete", "refs/alias/direct"],
            vec!["symbolic-ref", "--delete", "-q", "refs/alias/direct"],
            vec!["symbolic-ref", "--delete", "refs/alias/missing"],
            vec!["symbolic-ref", "--delete", "-q", "refs/alias/missing"],
            vec!["symbolic-ref", "--delete", "HEAD"],
            vec!["symbolic-ref", "--delete"],
            vec!["symbolic-ref", "--delete", "refs/alias/two", "extra"],
        ] {
            for repo in [&expected, &actual] {
                fs::create_dir_all(repo.join(".git").join("refs").join("alias"))
                    .expect("create alias refs dir");
                fs::write(
                    repo.join(".git").join("refs").join("alias").join("two"),
                    b"ref: refs/heads/main\n",
                )
                .expect("write symbolic alias");
                fs::write(
                    repo.join(".git").join("refs").join("alias").join("direct"),
                    b"0000000000000000000000000000000000000000\n",
                )
                .expect("write direct alias");
            }

            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
            assert_eq!(
                ref_exists(&actual, "refs/alias/two"),
                ref_exists(&expected, "refs/alias/two"),
                "symbolic ref existence differed for {args:?}"
            );
            assert_eq!(
                ref_exists(&actual, "refs/alias/direct"),
                ref_exists(&expected, "refs/alias/direct"),
                "direct ref existence differed for {args:?}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_onelevel_names_match_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-onelevel");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);
        run_success("git", &expected, &["commit", "--allow-empty", "-qm", "seed"]);
        run_success("git", &actual, &["commit", "--allow-empty", "-qm", "seed"]);

        for args in [
            vec!["symbolic-ref", "NOTHEAD", "refs/heads/foo"],
            vec!["symbolic-ref", "-d", "NOTHEAD"],
            vec!["symbolic-ref", "-d", "FOO"],
            vec!["symbolic-ref", "refs/heads/top-level", "ORIG_HEAD"],
            vec!["symbolic-ref", "refs/heads/invalid", "foo..bar"],
        ] {
            let expected_output = run("git", &expected, &args);
            let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
            assert_same_output(actual_output, expected_output, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_short_edge_cases_match_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-short-edge");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success("git", &root, &["init", "-q"]);
        run_success("git", &root, &["commit", "--allow-empty", "-qm", "seed"]);

        for (set_args, read_args) in [
            (
                vec!["symbolic-ref", "TEST_SYMREF", "refs/remotes/origin/HEAD"],
                vec!["symbolic-ref", "--short", "TEST_SYMREF"],
            ),
            (
                vec!["symbolic-ref", "TEST_SYMREF", "refs/headsXfoo"],
                vec!["symbolic-ref", "--short", "TEST_SYMREF"],
            ),
            (
                vec!["symbolic-ref", "TEST_SYMREF", "refs/heads/%foo"],
                vec!["symbolic-ref", "--short", "TEST_SYMREF"],
            ),
            (
                vec![
                    "symbolic-ref",
                    "TEST_SYMREF",
                    "refs/heads/测试-加-增加-加-增加",
                ],
                vec!["symbolic-ref", "--short", "TEST_SYMREF"],
            ),
        ] {
            run_success("git", &root, &set_args);
            let expected = run("git", &root, &read_args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &read_args);
            assert_same_output(actual, expected, &read_args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_df_conflict_matches_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-df");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        run_success("git", &expected, &["init", "-q"]);
        run_success("git", &actual, &["init", "-q"]);
        for repo in [&expected, &actual] {
            run_success("git", repo, &["commit", "--allow-empty", "-qm", "seed"]);
            run_success("git", repo, &["checkout", "-b", "df"]);
            run_success("git", repo, &["commit", "--allow-empty", "-qm", "df"]);
        }

        let args = ["symbolic-ref", "refs/heads/df/conflict", "refs/heads/df"];
        let expected_output = run("git", &expected, &args);
        let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

fn assert_symbolic_ref_matches_git(root: &Path, setup: impl Fn(&Path, &str)) {
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    run_success("git", &expected, &["init", "-q"]);
    run_success("git", &actual, &["init", "-q"]);
    run_success("git", &expected, &["commit", "--allow-empty", "-qm", "seed"]);
    run_success("git", &actual, &["commit", "--allow-empty", "-qm", "seed"]);
    let head = run_success("git", &expected, &["rev-parse", "HEAD"]);
    let oid = String::from_utf8_lossy(&head).trim().to_string();
    setup(&expected, &oid);
    setup(&actual, &oid);
    let args = ["symbolic-ref", "HEAD"];
    let expected_output = run("git", &expected, &args);
    let actual_output = run(env!("CARGO_BIN_EXE_sley"), &actual, &args);
    assert_same_output(actual_output, expected_output, &args);
}

#[test]
fn symbolic_ref_df_resolution_matches_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-df-resolve");
    fs::create_dir_all(&root).expect("create temp root");
    {
        assert_symbolic_ref_matches_git(&root.join("eisdir"), |repo, oid| {
            run_success("git", repo, &["symbolic-ref", "HEAD", "refs/heads/outer"]);
            fs::create_dir_all(repo.join(".git/refs/heads/outer")).expect("outer dir");
            fs::write(
                repo.join(".git/refs/heads/outer/inner"),
                format!("{oid}\n"),
            )
            .expect("inner ref");
        });
        assert_symbolic_ref_matches_git(&root.join("enotdir"), |repo, oid| {
            run_success("git", repo, &["symbolic-ref", "HEAD", "refs/heads/outer/inner"]);
            run_success("git", repo, &["update-ref", "refs/heads/outer", oid]);
        });
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbolic_ref_short_and_no_recurse_match_upstream_git() {
    let root = unique_temp_dir("symbolic-ref-short-no-recurse");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success("git", &root, &["init", "-q"]);
        fs::create_dir_all(root.join(".git").join("refs").join("meta"))
            .expect("create meta refs dir");
        fs::create_dir_all(root.join(".git").join("refs").join("alias"))
            .expect("create alias refs dir");
        fs::write(
            root.join(".git").join("refs").join("meta").join("base"),
            b"ref: refs/heads/main\n",
        )
        .expect("write base symbolic ref");
        fs::write(
            root.join(".git").join("refs").join("alias").join("two"),
            b"ref: refs/meta/base\n",
        )
        .expect("write chained symbolic ref");

        for args in [
            vec!["symbolic-ref", "refs/alias/two"],
            vec!["symbolic-ref", "--", "refs/alias/two"],
            vec!["symbolic-ref", "--no-recurse", "refs/alias/two"],
            vec![
                "symbolic-ref",
                "--no-recurse",
                "--recurse",
                "refs/alias/two",
            ],
            vec![
                "symbolic-ref",
                "--recurse",
                "--no-recurse",
                "refs/alias/two",
            ],
            vec!["symbolic-ref", "--short", "refs/alias/two"],
            vec!["symbolic-ref", "--short", "--no-short", "refs/alias/two"],
            vec!["symbolic-ref", "--no-short", "--short", "refs/alias/two"],
            vec!["symbolic-ref", "--short", "--no-recurse", "refs/alias/two"],
            vec!["symbolic-ref", "--no-recurse", "--short", "refs/alias/two"],
            vec!["symbolic-ref", "--short", "refs/heads/main"],
            vec!["symbolic-ref", "refs/heads/main"],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
