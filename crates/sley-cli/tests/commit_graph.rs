use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use sley_core::ObjectFormat;
use sley_formats::CommitGraph;

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

fn run_with_env(program: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
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

fn run_success_with_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Vec<u8> {
    let output = run_with_env(program, cwd, args, envs);
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

fn create_commit_graph_fixture(root: &Path) {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["config", "user.name", "Example User"],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["config", "user.email", "example@example.invalid"],
    );
    for name in ["one", "two", "three"] {
        fs::write(root.join(format!("{name}.txt")), format!("{name}\n")).expect("write fixture");
        run_success(sley_testkit::oracle_git(), root, &["add", "."]);
        run_success(
            sley_testkit::oracle_git(),
            root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", name],
        );
    }
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["commit-graph", "write", "--reachable"],
    );
    assert!(
        root.join(".git")
            .join("objects")
            .join("info")
            .join("commit-graph")
            .exists(),
        "upstream did not write commit-graph"
    );
}

fn create_commit_graph_fixture_with_env(root: &Path, envs: &[(&str, &str)]) {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["config", "user.name", "Example User"],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["config", "user.email", "example@example.invalid"],
    );
    for name in ["one", "two", "three"] {
        fs::write(root.join(format!("{name}.txt")), format!("{name}\n")).expect("write fixture");
        run_success_with_env(sley_testkit::oracle_git(), root, &["add", "."], envs);
        run_success_with_env(
            sley_testkit::oracle_git(),
            root,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", name],
            envs,
        );
    }
}

fn remove_single_commit_graph(root: &Path) {
    let graph_path = root
        .join(".git")
        .join("objects")
        .join("info")
        .join("commit-graph");
    if graph_path.exists() {
        fs::remove_file(graph_path).expect("remove commit-graph");
    }
}

fn single_commit_graph_path(root: &Path) -> PathBuf {
    root.join(".git")
        .join("objects")
        .join("info")
        .join("commit-graph")
}

fn single_commit_graph_bloom_hash_version(root: &Path) -> Option<u32> {
    let graph = CommitGraph::parse(
        &fs::read(single_commit_graph_path(root)).expect("read commit-graph"),
        ObjectFormat::Sha1,
    )
    .expect("parse commit-graph");
    graph.bloom_filters.map(|filters| filters.hash_version)
}

#[test]
fn commit_graph_verify_matches_upstream_git() {
    let root = unique_temp_dir("commit-graph-verify");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_commit_graph_fixture(&root);
        for args in [
            vec!["commit-graph", "verify"],
            vec!["commit-graph", "verify", "--object-dir=.git/objects"],
            vec!["commit-graph", "verify", "--no-progress"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_verify_split_chain_matches_upstream_git() {
    let root = unique_temp_dir("commit-graph-verify-split");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_commit_graph_fixture(&root);
        remove_single_commit_graph(&root);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["commit-graph", "write", "--reachable", "--split"],
        );
        assert!(
            root.join(".git")
                .join("objects")
                .join("info")
                .join("commit-graphs")
                .join("commit-graph-chain")
                .exists(),
            "upstream did not write split commit-graph chain"
        );

        for args in [
            vec!["commit-graph", "verify"],
            vec!["commit-graph", "verify", "--object-dir=.git/objects"],
            vec!["commit-graph", "verify", "--no-progress"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, &args);
            let actual = run(sley_testkit::sley_bin!(), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_write_reachable_writes_upstream_verifiable_graph() {
    let root = unique_temp_dir("commit-graph-write");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_commit_graph_fixture(&root);
        let graph_path = root
            .join(".git")
            .join("objects")
            .join("info")
            .join("commit-graph");
        fs::remove_file(&graph_path).expect("remove upstream commit-graph");

        let args = ["commit-graph", "write", "--reachable"];
        let actual = run(sley_testkit::sley_bin!(), &root, &args);
        assert!(
            actual.status.success(),
            "sley {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            actual.status.code(),
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&actual.stderr)
        );
        assert!(actual.stdout.is_empty(), "unexpected stdout");
        assert!(actual.stderr.is_empty(), "unexpected stderr");
        assert!(graph_path.exists(), "sley did not write commit-graph");
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["commit-graph", "verify"],
        );

        let expected = run(sley_testkit::oracle_git(), &root, &args);
        fs::remove_file(&graph_path).expect("remove upstream commit-graph");
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &[
                "commit-graph",
                "write",
                "--reachable",
                "--object-dir=.git/objects",
            ],
        );
        assert_same_output(actual, expected, &args);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["commit-graph", "verify"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_write_honors_changed_paths_version_config() {
    let root = unique_temp_dir("commit-graph-changed-paths-version");
    fs::create_dir_all(&root).expect("create temp repo root");
    {
        for (name, config_value, expected_hash_version) in
            [("v1", "1", 1), ("v2", "2", 2), ("disabled-read", "0", 1)]
        {
            let repo = root.join(name);
            fs::create_dir_all(&repo).expect("create temp repo");
            create_commit_graph_fixture(&repo);
            remove_single_commit_graph(&repo);
            run_success(
                sley_testkit::oracle_git(),
                &repo,
                &["config", "commitGraph.changedPathsVersion", config_value],
            );

            let args = ["commit-graph", "write", "--reachable", "--changed-paths"];
            let output = run(sley_testkit::sley_bin!(), &repo, &args);
            assert!(
                output.status.success(),
                "sley {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                single_commit_graph_bloom_hash_version(&repo),
                Some(expected_hash_version),
                "unexpected Bloom hash version for commitGraph.changedPathsVersion={config_value}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_write_autodetect_preserves_existing_changed_paths_version() {
    let root = unique_temp_dir("commit-graph-changed-paths-version-autodetect");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        create_commit_graph_fixture(&root);
        remove_single_commit_graph(&root);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["config", "commitGraph.changedPathsVersion", "2"],
        );
        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["commit-graph", "write", "--reachable", "--changed-paths"],
        );
        assert_eq!(single_commit_graph_bloom_hash_version(&root), Some(2));

        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["config", "commitGraph.changedPathsVersion", "-1"],
        );
        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["commit-graph", "write", "--reachable"],
        );
        assert_eq!(single_commit_graph_bloom_hash_version(&root), Some(2));
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_write_unsupported_changed_paths_version_is_warning_noop() {
    let root = unique_temp_dir("commit-graph-unsupported-changed-paths-version");
    fs::create_dir_all(&root).expect("create temp repo root");
    {
        for config_value in ["-2", "3"] {
            let repo = root.join(config_value.replace('-', "minus"));
            fs::create_dir_all(&repo).expect("create temp repo");
            create_commit_graph_fixture(&repo);
            remove_single_commit_graph(&repo);
            run_success(
                sley_testkit::oracle_git(),
                &repo,
                &["config", "commitGraph.changedPathsVersion", config_value],
            );

            let args = ["commit-graph", "write", "--reachable", "--changed-paths"];
            let output = run(sley_testkit::sley_bin!(), &repo, &args);
            assert!(
                output.status.success(),
                "unsupported changedPathsVersion should be a warning/no-op"
            );
            assert!(output.stdout.is_empty(), "unexpected stdout");
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                format!(
                    "warning: attempting to write a commit-graph, but 'commitGraph.changedPathsVersion' ({config_value}) is not supported\n"
                )
            );
            assert!(
                !single_commit_graph_path(&repo).exists(),
                "unsupported changedPathsVersion must not write a commit-graph"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_write_without_selector_matches_upstream_noop() {
    let root = unique_temp_dir("commit-graph-write-no-selector");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        for repo in [&expected, &actual] {
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &["init", "-q", "-b", "main"],
            );
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    "one",
                ],
            );
        }

        let args = ["commit-graph", "write"];
        let expected_output = run(sley_testkit::oracle_git(), &expected, &args);
        let actual_output = run(sley_testkit::sley_bin!(), &actual, &args);
        assert_same_output(actual_output, expected_output, &args);
        for repo in [&expected, &actual] {
            assert!(
                !repo
                    .join(".git")
                    .join("objects")
                    .join("info")
                    .join("commit-graph")
                    .exists(),
                "commit-graph write without a selector should not write a graph"
            );
        }

        let empty_expected = root.join("empty-expected");
        let empty_actual = root.join("empty-actual");
        fs::create_dir_all(&empty_expected).expect("create empty expected repo dir");
        fs::create_dir_all(&empty_actual).expect("create empty actual repo dir");
        run_success(
            sley_testkit::oracle_git(),
            &empty_expected,
            &["init", "-q", "-b", "main"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &empty_actual,
            &["init", "-q", "-b", "main"],
        );
        let expected_output = run(sley_testkit::oracle_git(), &empty_expected, &args);
        let actual_output = run(sley_testkit::sley_bin!(), &empty_actual, &args);
        assert_same_output(actual_output, expected_output, &args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn commit_graph_git_object_directory_default_matches_upstream_git() {
    let root = unique_temp_dir("commit-graph-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        for repo in [&expected, &actual] {
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom objects dir");
            create_commit_graph_fixture_with_env(repo, &envs);
        }

        let args = ["commit-graph", "write", "--reachable"];
        let expected_output = run_with_env(sley_testkit::oracle_git(), &expected, &args, &envs);
        let actual_output = run_with_env(sley_testkit::sley_bin!(), &actual, &args, &envs);
        assert_same_output(actual_output, expected_output, &args);

        for repo in [&expected, &actual] {
            assert!(
                repo.join("custom-objects")
                    .join("info")
                    .join("commit-graph")
                    .exists(),
                "commit-graph was not written to GIT_OBJECT_DIRECTORY"
            );
            assert!(
                !repo
                    .join(".git")
                    .join("objects")
                    .join("info")
                    .join("commit-graph")
                    .exists(),
                "commit-graph was written to the default object directory"
            );
        }

        let verify_args = ["commit-graph", "verify"];
        let expected_verify =
            run_with_env(sley_testkit::oracle_git(), &expected, &verify_args, &envs);
        let actual_verify = run_with_env(sley_testkit::sley_bin!(), &actual, &verify_args, &envs);
        assert_same_output(actual_verify, expected_verify, &verify_args);
        run_success_with_env(sley_testkit::oracle_git(), &actual, &verify_args, &envs);
    };
    let _ = fs::remove_dir_all(&root);
}
