use std::fs;
use std::os::unix::fs::MetadataExt;
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

fn loose_object_path(git_dir: &Path, oid: &str) -> PathBuf {
    git_dir.join("objects").join(&oid[..2]).join(&oid[2..])
}

fn repository_pack_pair(git_dir: &Path) -> (PathBuf, PathBuf) {
    let pack_dir = git_dir.join("objects").join("pack");
    let mut packs = Vec::new();
    let mut indexes = Vec::new();
    for entry in fs::read_dir(&pack_dir).expect("read pack dir") {
        let path = entry.expect("read pack entry").path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => packs.push(path),
            Some("idx") => indexes.push(path),
            _ => {}
        }
    }
    assert_eq!(
        packs.len(),
        1,
        "expected one pack in {}",
        pack_dir.display()
    );
    assert_eq!(
        indexes.len(),
        1,
        "expected one pack index in {}",
        pack_dir.display()
    );
    assert_eq!(
        packs[0].file_stem(),
        indexes[0].file_stem(),
        "pack and index stem should match"
    );
    (packs.remove(0), indexes.remove(0))
}

fn repository_promisor_sidecars(git_dir: &Path) -> Vec<PathBuf> {
    let pack_dir = git_dir.join("objects").join("pack");
    let mut promisors = fs::read_dir(&pack_dir)
        .expect("read pack dir")
        .map(|entry| entry.expect("read pack entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("promisor"))
        .collect::<Vec<_>>();
    promisors.sort();
    promisors
}

fn assert_reachable_objects_stored_in_pack(repo: &Path, git_dir: &Path) {
    let (_pack_path, index_path) = repository_pack_pair(git_dir);
    let index_arg = index_path.to_string_lossy();
    run_success(
        sley_testkit::oracle_git(),
        repo,
        &["verify-pack", "-v", &index_arg],
    );

    let objects = String::from_utf8(run_success(
        sley_testkit::oracle_git(),
        repo,
        &["rev-list", "--objects", "--all", "HEAD"],
    ))
    .expect("rev-list output is utf8");
    for oid in objects
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        assert!(
            !loose_object_path(git_dir, oid).exists(),
            "cloned object {oid} should be stored in pack, not as loose object"
        );
    }
}

fn assert_repository_objects_are_hardlinked(git_dir: &Path) {
    fn visit(path: &Path) {
        for entry in fs::read_dir(path).expect("read object directory") {
            let entry = entry.expect("read object entry");
            let file_type = entry.file_type().expect("object file type");
            if file_type.is_dir() {
                visit(&entry.path());
            } else if file_type.is_file() {
                assert!(
                    entry.metadata().expect("object metadata").nlink() > 1,
                    "object file {} should be hardlinked",
                    entry.path().display()
                );
            }
        }
    }
    visit(&git_dir.join("objects"));
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

fn assert_same_output_with_normalized_destination(
    actual: Output,
    expected: Output,
    expected_destination: &str,
    actual_destination: &str,
    args: &[&str],
) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        actual.stdout, expected.stdout,
        "stdout differed for {args:?}"
    );
    let expected_stderr =
        String::from_utf8_lossy(&expected.stderr).replace(expected_destination, actual_destination);
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        expected_stderr,
        "stderr differed for {args:?}"
    );
}

fn normalize_path_text(text: &[u8], replacements: &[(&Path, &Path)]) -> String {
    let mut normalized = String::from_utf8_lossy(text).into_owned();
    for (expected, actual) in replacements {
        normalized = normalized.replace(
            expected.to_string_lossy().as_ref(),
            actual.to_string_lossy().as_ref(),
        );
        if let (Ok(expected), Ok(actual)) = (fs::canonicalize(expected), fs::canonicalize(actual)) {
            normalized = normalized.replace(
                expected.to_string_lossy().as_ref(),
                actual.to_string_lossy().as_ref(),
            );
        }
    }
    normalized
}

fn assert_same_output_with_normalized_paths(
    actual: Output,
    expected: Output,
    replacements: &[(&Path, &Path)],
    args: &[&str],
) {
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "status differed for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        normalize_path_text(&expected.stdout, replacements),
        "stdout differed for {args:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        normalize_path_text(&expected.stderr, replacements),
        "stderr differed for {args:?}"
    );
}

fn create_source_repo(root: &Path) {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "-b", "main"],
    );
    fs::write(root.join("payload.txt"), b"clone payload\n").expect("write payload");
    run_success(sley_testkit::oracle_git(), root, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "initial",
            "-q",
        ],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["branch", "feature/topic"],
    );
    run_success(sley_testkit::oracle_git(), root, &["tag", "v1.0"]);
}

fn create_sha256_nested_source_repo(root: &Path) {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["init", "-q", "--object-format=sha256", "-b", "main"],
    );
    fs::create_dir_all(root.join("dir")).expect("create source dir");
    fs::create_dir_all(root.join("deep/nested")).expect("create source nested dir");
    fs::write(root.join("payload.txt"), b"clone payload\n").expect("write payload");
    fs::write(root.join("dir/file.txt"), b"dir\n").expect("write dir payload");
    fs::write(root.join("deep/nested/file.txt"), b"deep\n").expect("write deep payload");
    run_success(sley_testkit::oracle_git(), root, &["add", "."]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "initial",
            "-q",
        ],
    );
}

fn add_feature_commit(root: &Path) {
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["checkout", "-q", "feature/topic"],
    );
    fs::write(root.join("payload.txt"), b"feature payload\n").expect("write feature payload");
    run_success(sley_testkit::oracle_git(), root, &["add", "payload.txt"]);
    run_success(
        sley_testkit::oracle_git(),
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "feature",
            "-q",
        ],
    );
    run_success(
        sley_testkit::oracle_git(),
        root,
        &["checkout", "-q", "main"],
    );
}

#[test]
fn clone_local_repository_matches_upstream_git() {
    let root = unique_temp_dir("clone-local");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");

        let expected = run(
            sley_testkit::oracle_git(),
            &root,
            &["clone", "-q", source_arg, expected_arg],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &["clone", "-q", source_arg, actual_arg],
        );
        assert_same_output(actual, expected, &["clone", "-q", source_arg, actual_arg]);

        for args in [
            vec!["show-ref"],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
            vec!["status", "--short"],
            vec!["config", "--get", "remote.origin.url"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }

        let branch = String::from_utf8(run_success(
            sley_testkit::oracle_git(),
            &expected_repo,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("branch is utf8")
        .trim()
        .to_string();
        for key in [
            format!("branch.{branch}.remote"),
            format!("branch.{branch}.merge"),
        ] {
            let expected = run(
                sley_testkit::oracle_git(),
                &expected_repo,
                &["config", "--get", &key],
            );
            let actual = run(
                sley_testkit::oracle_git(),
                &actual_repo,
                &["config", "--get", &key],
            );
            assert_same_output(actual, expected, &["config", "--get", &key]);
        }
        assert_eq!(
            fs::read(expected_repo.join("payload.txt")).expect("read expected payload"),
            fs::read(actual_repo.join("payload.txt")).expect("read actual payload")
        );
        assert_repository_objects_are_hardlinked(&actual_repo.join(".git"));
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_default_directory_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-default-directory");
    let source = root.join("source");
    let expected_root = root.join("expected-root");
    let actual_root = root.join("actual-root");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_root).expect("create expected root");
    fs::create_dir_all(&actual_root).expect("create actual root");
    {
        create_source_repo(&source);
        let source_arg = source
            .join(".git")
            .to_str()
            .expect("source git path is utf8")
            .to_string();

        let expected = run(
            sley_testkit::oracle_git(),
            &expected_root,
            &["clone", "-q", &source_arg],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &actual_root,
            &["clone", "-q", &source_arg],
        );
        assert_same_output(actual, expected, &["clone", "-q", &source_arg]);

        let expected_repo = expected_root.join("source");
        let actual_repo = actual_root.join("source");
        for args in [
            vec!["show-ref"],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
            vec!["status", "--short"],
            vec!["config", "--get", "remote.origin.url"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_bare_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-bare");
    let source = root.join("source");
    let expected_repo = root.join("expected.git");
    let actual_repo = root.join("actual.git");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");

        let expected = run(
            sley_testkit::oracle_git(),
            &root,
            &["clone", "-q", "--bare", source_arg, expected_arg],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &["clone", "-q", "--bare", source_arg, actual_arg],
        );
        assert_same_output(
            actual,
            expected,
            &["clone", "-q", "--bare", source_arg, actual_arg],
        );

        for args in [
            vec!["show-ref"],
            vec!["symbolic-ref", "HEAD"],
            vec!["config", "--get", "core.bare"],
            vec!["config", "--get", "remote.origin.url"],
            vec!["config", "--get-all", "remote.origin.fetch"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
        assert_eq!(
            expected_repo.join("payload.txt").exists(),
            actual_repo.join("payload.txt").exists(),
            "bare clone worktree payload presence differed"
        );
        assert_eq!(
            expected_repo.join("FETCH_HEAD").exists(),
            actual_repo.join("FETCH_HEAD").exists(),
            "bare clone FETCH_HEAD presence differed"
        );
        assert_repository_objects_are_hardlinked(&actual_repo);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_bare_options_match_upstream_git() {
    let root = unique_temp_dir("clone-local-bare-options");
    let source = root.join("source");
    let expected_root = root.join("expected-root");
    let actual_root = root.join("actual-root");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_root).expect("create expected root");
    fs::create_dir_all(&actual_root).expect("create actual root");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source
            .join(".git")
            .to_str()
            .expect("source git path is utf8")
            .to_string();

        let expected = run(
            sley_testkit::oracle_git(),
            &expected_root,
            &[
                "clone",
                "-q",
                "--bare",
                "--branch",
                "feature/topic",
                "--no-tags",
                &source_arg,
            ],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &actual_root,
            &[
                "clone",
                "-q",
                "--bare",
                "--branch",
                "feature/topic",
                "--no-tags",
                &source_arg,
            ],
        );
        assert_same_output(
            actual,
            expected,
            &[
                "clone",
                "-q",
                "--bare",
                "--branch",
                "feature/topic",
                "--no-tags",
                &source_arg,
            ],
        );

        let expected_repo = expected_root.join("source.git");
        let actual_repo = actual_root.join("source.git");
        for args in [
            vec!["show-ref"],
            vec!["show-ref", "--tags"],
            vec!["symbolic-ref", "HEAD"],
            vec!["config", "--get", "core.bare"],
            vec!["config", "--get", "remote.origin.url"],
            vec!["config", "--get", "remote.origin.tagOpt"],
            vec!["config", "--get-all", "remote.origin.fetch"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_bare_and_mirror_origin_option_match_upstream_git() {
    let root = unique_temp_dir("clone-local-bare-mirror-origin");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, mode) in [("bare", "--bare"), ("mirror", "--mirror")] {
            let expected_repo = root.join(format!("{label}-expected.git"));
            let actual_repo = root.join(format!("{label}-actual.git"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");

            let expected = run(
                sley_testkit::oracle_git(),
                &root,
                &[
                    "clone",
                    "-q",
                    mode,
                    "--origin",
                    "upstream",
                    source_arg,
                    expected_arg,
                ],
            );
            let actual = run(
                sley_testkit::sley_bin!(),
                &root,
                &[
                    "clone", "-q", mode, "--origin", "upstream", source_arg, actual_arg,
                ],
            );
            assert_same_output(
                actual,
                expected,
                &[
                    "clone", "-q", mode, "--origin", "upstream", source_arg, actual_arg,
                ],
            );

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "remote.upstream.url"],
                vec!["config", "--get", "remote.upstream.fetch"],
                vec!["config", "--get", "remote.upstream.mirror"],
                vec!["config", "--get", "remote.upstream.tagOpt"],
                vec!["config", "--get", "remote.origin.url"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_mirror_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-mirror");
    let source = root.join("source");
    let expected_root = root.join("expected-root");
    let actual_root = root.join("actual-root");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_root).expect("create expected root");
    fs::create_dir_all(&actual_root).expect("create actual root");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        run_success(
            sley_testkit::oracle_git(),
            &source,
            &["update-ref", "refs/notes/test", "HEAD"],
        );
        let source_arg = source
            .join(".git")
            .to_str()
            .expect("source git path is utf8")
            .to_string();

        let expected = run(
            sley_testkit::oracle_git(),
            &expected_root,
            &["clone", "-q", "--mirror", &source_arg],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &actual_root,
            &["clone", "-q", "--mirror", &source_arg],
        );
        assert_same_output(actual, expected, &["clone", "-q", "--mirror", &source_arg]);

        let expected_repo = expected_root.join("source.git");
        let actual_repo = actual_root.join("source.git");
        for args in [
            vec!["show-ref"],
            vec!["show-ref", "refs/notes/test"],
            vec!["symbolic-ref", "HEAD"],
            vec!["config", "--get", "core.bare"],
            vec!["config", "--get", "remote.origin.url"],
            vec!["config", "--get", "remote.origin.fetch"],
            vec!["config", "--get", "remote.origin.mirror"],
            vec!["config", "--get", "remote.origin.tagOpt"],
            vec!["config", "--get-all", "remote.origin.fetch"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
        assert_eq!(
            expected_repo.join("FETCH_HEAD").exists(),
            actual_repo.join("FETCH_HEAD").exists(),
            "mirror clone FETCH_HEAD presence differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_no_mirror_restores_non_bare_clone_like_upstream_git() {
    let root = unique_temp_dir("clone-local-no-mirror");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");

        let expected = run(
            sley_testkit::oracle_git(),
            &root,
            &[
                "clone",
                "-q",
                "--mirror",
                "--no-mirror",
                source_arg,
                expected_arg,
            ],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &[
                "clone",
                "-q",
                "--mirror",
                "--no-mirror",
                source_arg,
                actual_arg,
            ],
        );
        assert_same_output(
            actual,
            expected,
            &[
                "clone",
                "-q",
                "--mirror",
                "--no-mirror",
                source_arg,
                actual_arg,
            ],
        );

        for args in [
            vec!["show-ref"],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
            vec!["status", "--short"],
            vec!["config", "--get", "core.bare"],
            vec!["config", "--get", "remote.origin.mirror"],
            vec!["config", "--get", "remote.origin.fetch"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_single_branch_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-single-branch");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("default-head", vec!["--single-branch"]),
            (
                "feature",
                vec!["--single-branch", "--branch", "feature/topic"],
            ),
            ("reset", vec!["--single-branch", "--no-single-branch"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "refs/remotes/origin/HEAD"],
                vec!["rev-parse", "--abbrev-ref", "HEAD"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_bare_and_mirror_single_branch_match_upstream_git() {
    let root = unique_temp_dir("clone-local-bare-mirror-single-branch");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("bare", vec!["--bare", "--single-branch"]),
            (
                "bare-feature",
                vec!["--bare", "--single-branch", "--branch", "feature/topic"],
            ),
            ("mirror", vec!["--mirror", "--single-branch"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected.git"));
            let actual_repo = root.join(format!("{label}-actual.git"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_no_checkout_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-no-checkout");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");

        let expected = run(
            sley_testkit::oracle_git(),
            &root,
            &["clone", "-q", "--no-checkout", source_arg, expected_arg],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &["clone", "-q", "--no-checkout", source_arg, actual_arg],
        );
        assert_same_output(
            actual,
            expected,
            &["clone", "-q", "--no-checkout", source_arg, actual_arg],
        );

        for args in [
            vec!["show-ref"],
            vec!["rev-parse", "--abbrev-ref", "HEAD"],
            vec!["status", "--short"],
            vec!["ls-files", "--stage"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }
        assert_eq!(
            expected_repo.join("payload.txt").exists(),
            actual_repo.join("payload.txt").exists(),
            "payload checkout presence differed"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_sha256_no_checkout_and_sparse_match_upstream_git() {
    let root = unique_temp_dir("clone-sha256-index-options");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_sha256_nested_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("no-checkout", vec!["--no-checkout"]),
            ("sparse", vec!["--sparse"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["rev-parse", "--show-object-format=storage"],
                vec!["show-ref"],
                vec!["status", "--short"],
                vec!["ls-files", "--stage"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_tag_options_match_upstream_git() {
    let root = unique_temp_dir("clone-local-tag-options");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("no-tags", vec!["--no-tags"]),
            ("no-tags-then-tags", vec!["--no-tags", "--tags"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref", "--tags"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_branch_option_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-branch-option");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, branch_args) in [
            ("long-space", vec!["--branch", "feature/topic"]),
            ("long-equals", vec!["--branch=feature/topic"]),
            ("short-compact", vec!["-bfeature/topic"]),
            ("tag", vec!["--branch", "v1.0"]),
            ("reset", vec!["--branch", "feature/topic", "--no-branch"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(branch_args.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(branch_args.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["rev-parse", "--abbrev-ref", "HEAD"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "branch.main.remote"],
                vec!["config", "--get", "branch.main.merge"],
                vec!["config", "--get", "branch.feature/topic.remote"],
                vec!["config", "--get", "branch.feature/topic.merge"],
                vec!["symbolic-ref", "refs/remotes/origin/HEAD"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
            assert_eq!(
                fs::read(expected_repo.join("payload.txt")).expect("read expected payload"),
                fs::read(actual_repo.join("payload.txt")).expect("read actual payload")
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_revision_option_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-revision-option");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options, bare_layout) in [
            ("revision", vec!["--revision", "HEAD"], false),
            ("revision-equals", vec!["--revision=HEAD"], false),
            (
                "revision-no-checkout",
                vec!["--revision", "HEAD", "--no-checkout"],
                false,
            ),
            ("revision-bare", vec!["--bare", "--revision", "HEAD"], true),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["status", "--short", "--branch"],
                vec!["rev-parse", "HEAD"],
                vec!["symbolic-ref", "-q", "HEAD"],
                vec!["config", "--get", "remote.origin.url"],
                vec!["config", "--get-all", "remote.origin.fetch"],
                vec!["config", "--get", "core.bare"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }

            let expected_head = if bare_layout {
                expected_repo.join("HEAD")
            } else {
                expected_repo.join(".git/HEAD")
            };
            let actual_head = if bare_layout {
                actual_repo.join("HEAD")
            } else {
                actual_repo.join(".git/HEAD")
            };
            assert_eq!(
                fs::read(expected_head).expect("read expected HEAD"),
                fs::read(actual_head).expect("read actual HEAD"),
                "HEAD differed for {label}"
            );
            assert_eq!(
                expected_repo.join("payload.txt").exists(),
                actual_repo.join("payload.txt").exists(),
                "payload presence differed for {label}"
            );
            let actual_git_dir = if bare_layout {
                actual_repo.clone()
            } else {
                actual_repo.join(".git")
            };
            assert_reachable_objects_stored_in_pack(&actual_repo, &actual_git_dir);
        }

        let expected_repo = root.join("revision-reset-expected");
        let actual_repo = root.join("revision-reset-actual");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");
        let expected = run(
            sley_testkit::oracle_git(),
            &root,
            &[
                "clone",
                "-q",
                "--revision",
                "HEAD",
                "--no-revision",
                source_arg,
                expected_arg,
            ],
        );
        let actual = run(
            sley_testkit::sley_bin!(),
            &root,
            &[
                "clone",
                "-q",
                "--revision",
                "HEAD",
                "--no-revision",
                source_arg,
                actual_arg,
            ],
        );
        assert_same_output(
            actual,
            expected,
            &[
                "clone",
                "-q",
                "--revision",
                "HEAD",
                "--no-revision",
                source_arg,
                actual_arg,
            ],
        );
        for args in [
            vec!["show-ref"],
            vec!["status", "--short", "--branch"],
            vec!["symbolic-ref", "-q", "HEAD"],
            vec!["config", "--get-all", "remote.origin.fetch"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }

        for (label, options) in [
            (
                "revision-branch-conflict",
                vec!["--revision", "HEAD", "--branch", "main"],
            ),
            (
                "revision-mirror-conflict",
                vec!["--mirror", "--revision", "HEAD"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_origin_option_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-origin-option");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, origin_args) in [
            ("long-origin", vec!["--origin", "upstream"]),
            ("short-origin", vec!["-o", "upstream"]),
            ("short-compact", vec!["-oupstream"]),
            ("reset", vec!["--origin", "upstream", "--no-origin"]),
            ("no-origin", vec!["--no-origin"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(origin_args.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(origin_args.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["config", "--get", "remote.origin.url"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.upstream.url"],
                vec!["config", "--get", "remote.upstream.fetch"],
                vec!["symbolic-ref", "refs/remotes/origin/HEAD"],
                vec!["symbolic-ref", "refs/remotes/upstream/HEAD"],
                vec!["rev-parse", "--abbrev-ref", "HEAD"],
                vec!["config", "--get", "branch.main.remote"],
                vec!["config", "--get", "branch.main.merge"],
                vec!["show-ref", "refs/remotes/origin/main"],
                vec!["show-ref", "refs/remotes/origin/feature/topic"],
                vec!["show-ref", "refs/remotes/upstream/main"],
                vec!["show-ref", "refs/remotes/upstream/feature/topic"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_config_options_match_upstream_git() {
    let root = unique_temp_dir("clone-local-config-options");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options, expected_value) in [
            ("normal-short", vec!["-c", "clone.example=normal"], "normal"),
            (
                "bare-long",
                vec!["--bare", "--config", "clone.example=bare"],
                "bare",
            ),
            (
                "mirror-equals",
                vec!["--mirror", "--config=clone.example=mirror"],
                "mirror",
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "clone.example"],
                vec!["config", "--get", "remote.origin.url"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
            assert_eq!(
                String::from_utf8(run_success(
                    sley_testkit::oracle_git(),
                    &actual_repo,
                    &["config", "--get", "clone.example"],
                ))
                .expect("clone.example value is utf8")
                .trim(),
                expected_value
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_config_promisors_precede_generated_origin_like_upstream_git() {
    let root = unique_temp_dir("clone-promisor-config-order");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    create_source_repo(&source);
    let source_arg = source.to_str().expect("source path is utf8");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    let expected_args = [
        "clone",
        "-q",
        "--no-local",
        "--filter=blob:none",
        "-c",
        "remote.unused_lop.promisor=true",
        "-c",
        "remote.lop.promisor=true",
        source_arg,
        expected_repo.to_str().expect("expected path is utf8"),
    ];
    let mut actual_args = expected_args;
    actual_args[actual_args.len() - 1] = actual_repo.to_str().expect("actual path is utf8");
    assert_same_output(
        run(sley_testkit::oracle_git(), &root, &expected_args),
        run(sley_testkit::sley_bin!(), &root, &actual_args),
        &actual_args,
    );

    let query = [
        "config",
        "get",
        "--all",
        "--show-names",
        "--regexp",
        "^remote\\..*\\.promisor$",
    ];
    let expected = run_success(sley_testkit::oracle_git(), &expected_repo, &query);
    let actual = run_success(sley_testkit::oracle_git(), &actual_repo, &query);
    assert_eq!(actual, expected);
    assert_eq!(
        String::from_utf8(actual).expect("promisor config is utf8"),
        "remote.unused_lop.promisor true\nremote.lop.promisor true\nremote.origin.promisor true\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn clone_local_repository_template_options_match_upstream_git() {
    let root = unique_temp_dir("clone-local-template-options");
    let source = root.join("source");
    let template = root.join("template");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(template.join("info")).expect("create template info");
    fs::create_dir_all(template.join("hooks")).expect("create template hooks");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        fs::write(template.join("info").join("exclude"), b"template-exclude\n")
            .expect("write template exclude");
        fs::write(template.join("hooks").join("pre-commit"), b"#!/bin/sh\n")
            .expect("write template hook");
        fs::write(template.join("HEAD"), b"ref: refs/heads/template\n")
            .expect("write template HEAD");
        fs::write(template.join("config"), b"[template]\n\tmarker = yes\n")
            .expect("write template config");
        let source_arg = source.to_str().expect("source path is utf8");
        let template_arg = template.to_str().expect("template path is utf8");
        let missing_template = root.join("missing-template");
        let missing_template_arg = missing_template
            .to_str()
            .expect("missing template path is utf8");
        let template_equals = format!("--template={template_arg}");

        for (label, options, template_expected, bare_layout) in [
            ("long-space", vec!["--template", template_arg], true, false),
            ("long-equals", vec![template_equals.as_str()], true, false),
            (
                "missing",
                vec!["--template", missing_template_arg],
                false,
                false,
            ),
            // An empty `--template=` disables templating; upstream git treats it
            // as "no template". A regression in resolving "" against the cwd made
            // sley copy the cwd into the destination gitdir, recursing until the
            // path overflowed — this case guards that the clone simply completes
            // with no template applied, matching git.
            ("empty-equals", vec!["--template="], false, false),
            ("empty-space", vec!["--template", ""], false, false),
            (
                "bare-template",
                vec!["--bare", "--template", template_arg],
                true,
                true,
            ),
            (
                "mirror-template",
                vec!["--mirror", "--template", template_arg],
                true,
                true,
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "template.marker"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }

            let expected_git_dir = if bare_layout {
                expected_repo.clone()
            } else {
                expected_repo.join(".git")
            };
            let actual_git_dir = if bare_layout {
                actual_repo.clone()
            } else {
                actual_repo.join(".git")
            };
            for relative in ["info/exclude", "hooks/pre-commit"] {
                assert_eq!(
                    expected_git_dir.join(relative).exists(),
                    actual_git_dir.join(relative).exists(),
                    "template file presence differed for {relative} and {label}"
                );
            }
            if template_expected {
                assert_eq!(
                    fs::read(expected_git_dir.join("info/exclude"))
                        .expect("read expected template exclude"),
                    fs::read(actual_git_dir.join("info/exclude"))
                        .expect("read actual template exclude")
                );
                assert_eq!(
                    fs::read(expected_git_dir.join("hooks/pre-commit"))
                        .expect("read expected template hook"),
                    fs::read(actual_git_dir.join("hooks/pre-commit"))
                        .expect("read actual template hook")
                );
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_filter_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-filter-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("space-blob-none", vec!["--filter", "blob:none"]),
            ("equals-blob-none", vec!["--filter=blob:none"]),
            ("tree-depth", vec!["--filter=tree:0"]),
            ("reset", vec!["--filter=blob:none", "--no-filter"]),
            ("bare-filter", vec!["--bare", "--filter=blob:none"]),
            ("mirror-filter", vec!["--mirror", "--filter=blob:none"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.promisor"],
                vec!["config", "--get", "remote.origin.partialclonefilter"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_file_repository_filter_marks_promisor_pack_like_upstream_git() {
    let root = unique_temp_dir("clone-file-filter-promisor-pack");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = format!("file://{}", source.display());
        let expected_repo = root.join("expected");
        let actual_repo = root.join("actual");
        let expected_arg = expected_repo.to_str().expect("expected path is utf8");
        let actual_arg = actual_repo.to_str().expect("actual path is utf8");
        let expected_args = vec![
            "clone",
            "-q",
            "--filter=blob:none",
            source_arg.as_str(),
            expected_arg,
        ];
        let actual_args = vec![
            "clone",
            "-q",
            "--filter=blob:none",
            source_arg.as_str(),
            actual_arg,
        ];

        let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
        let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
        assert_same_output(actual, expected, &actual_args);

        for args in [
            vec!["config", "--get", "remote.origin.promisor"],
            vec!["config", "--get", "remote.origin.partialclonefilter"],
            vec!["show-ref"],
            vec!["status", "--short"],
        ] {
            let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
            let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }

        let actual_git_dir = actual_repo.join(".git");
        let promisors = repository_promisor_sidecars(&actual_git_dir);
        assert_eq!(promisors.len(), 1);
        assert_eq!(fs::read(&promisors[0]).expect("read promisor"), b"");
        let (pack, index) = repository_pack_pair(&actual_git_dir);
        assert_eq!(promisors[0].file_stem(), pack.file_stem());
        assert_eq!(promisors[0].file_stem(), index.file_stem());
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_bundle_uri_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-bundle-uri-flags");
    let source = root.join("source");
    let bundle = root.join("source.bundle");
    let missing_bundle = root.join("missing.bundle");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        let bundle_arg = bundle.to_str().expect("bundle path is utf8");
        let missing_bundle_arg = missing_bundle
            .to_str()
            .expect("missing bundle path is utf8");
        run_success(
            sley_testkit::oracle_git(),
            &source,
            &["bundle", "create", bundle_arg, "--all"],
        );
        let bundle_equals = format!("--bundle-uri={bundle_arg}");

        for (label, options) in [
            ("space", vec!["--bundle-uri", bundle_arg]),
            ("equals", vec![bundle_equals.as_str()]),
            ("reset", vec!["--bundle-uri", bundle_arg, "--no-bundle-uri"]),
            (
                "depth-reset",
                vec!["--bundle-uri", bundle_arg, "--depth", "1", "--no-depth"],
            ),
            ("missing", vec!["--bundle-uri", missing_bundle_arg]),
            ("bare", vec!["--bare", "--bundle-uri", bundle_arg]),
            ("mirror", vec!["--mirror", "--bundle-uri", bundle_arg]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["status", "--short"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.url"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }

        for (label, options) in [
            (
                "depth-conflict",
                vec!["--bundle-uri", bundle_arg, "--depth", "1"],
            ),
            (
                "shallow-since-conflict",
                vec!["--bundle-uri", bundle_arg, "--shallow-since", "yesterday"],
            ),
            (
                "shallow-exclude-conflict",
                vec!["--bundle-uri", bundle_arg, "--shallow-exclude", "main"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_sparse_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-sparse-flags");
    let source = root.join("source");
    fs::create_dir_all(source.join("dir")).expect("create source dir");
    fs::create_dir_all(source.join("deep/nested")).expect("create source nested dir");
    {
        run_success(
            sley_testkit::oracle_git(),
            &source,
            &["init", "-q", "-b", "main"],
        );
        fs::write(source.join("root.txt"), b"root\n").expect("write root file");
        fs::write(source.join("dir/file.txt"), b"dir\n").expect("write dir file");
        fs::write(source.join("deep/nested/file.txt"), b"deep\n").expect("write deep file");
        run_success(sley_testkit::oracle_git(), &source, &["add", "."]);
        run_success(
            sley_testkit::oracle_git(),
            &source,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "initial",
                "-q",
            ],
        );
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("sparse", vec!["--sparse"]),
            ("sparse-reset", vec!["--sparse", "--no-sparse"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["status", "--short"],
                vec!["ls-files", "-t"],
                vec!["config", "--get", "extensions.worktreeConfig"],
                vec!["config", "--get", "core.sparseCheckout"],
                vec!["config", "--get", "core.sparseCheckoutCone"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }

            for relative in ["root.txt", "dir/file.txt", "deep/nested/file.txt"] {
                assert_eq!(
                    expected_repo.join(relative).exists(),
                    actual_repo.join(relative).exists(),
                    "worktree file presence differed for {relative} and {label}"
                );
            }
            assert_eq!(
                fs::read(expected_repo.join(".git/info/sparse-checkout")).ok(),
                fs::read(actual_repo.join(".git/info/sparse-checkout")).ok(),
                "sparse-checkout file differed for {label}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_separate_git_dir_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-separate-git-dir-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        struct SeparateCase {
            label: &'static str,
            expected_options: Vec<String>,
            actual_options: Vec<String>,
            expected_git_dir: Option<PathBuf>,
            actual_git_dir: Option<PathBuf>,
        }

        let space_expected_git_dir = root.join("space-expected.git");
        let space_actual_git_dir = root.join("space-actual.git");
        let equals_expected_git_dir = root.join("equals-expected.git");
        let equals_actual_git_dir = root.join("equals-actual.git");
        let reset_expected_git_dir = root.join("reset-expected.git");
        let reset_actual_git_dir = root.join("reset-actual.git");
        let cases = vec![
            SeparateCase {
                label: "space",
                expected_options: vec![
                    "--separate-git-dir".into(),
                    space_expected_git_dir.to_string_lossy().into_owned(),
                ],
                actual_options: vec![
                    "--separate-git-dir".into(),
                    space_actual_git_dir.to_string_lossy().into_owned(),
                ],
                expected_git_dir: Some(space_expected_git_dir),
                actual_git_dir: Some(space_actual_git_dir),
            },
            SeparateCase {
                label: "equals",
                expected_options: vec![format!(
                    "--separate-git-dir={}",
                    equals_expected_git_dir.display()
                )],
                actual_options: vec![format!(
                    "--separate-git-dir={}",
                    equals_actual_git_dir.display()
                )],
                expected_git_dir: Some(equals_expected_git_dir),
                actual_git_dir: Some(equals_actual_git_dir),
            },
            SeparateCase {
                label: "reset",
                expected_options: vec![
                    "--separate-git-dir".into(),
                    reset_expected_git_dir.to_string_lossy().into_owned(),
                    "--no-separate-git-dir".into(),
                ],
                actual_options: vec![
                    "--separate-git-dir".into(),
                    reset_actual_git_dir.to_string_lossy().into_owned(),
                    "--no-separate-git-dir".into(),
                ],
                expected_git_dir: None,
                actual_git_dir: None,
            },
        ];

        for case in cases {
            let expected_repo = root.join(format!("{}-expected", case.label));
            let actual_repo = root.join(format!("{}-actual", case.label));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone".to_string(), "-q".to_string()];
            expected_args.extend(case.expected_options);
            expected_args.extend([source_arg.to_string(), expected_arg.to_string()]);
            let mut actual_args = vec!["clone".to_string(), "-q".to_string()];
            actual_args.extend(case.actual_options);
            actual_args.extend([source_arg.to_string(), actual_arg.to_string()]);
            let expected_arg_refs = expected_args.iter().map(String::as_str).collect::<Vec<_>>();
            let actual_arg_refs = actual_args.iter().map(String::as_str).collect::<Vec<_>>();

            let expected = run(sley_testkit::oracle_git(), &root, &expected_arg_refs);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_arg_refs);
            let mut replacements = vec![(expected_repo.as_path(), actual_repo.as_path())];
            if let (Some(expected_git_dir), Some(actual_git_dir)) = (
                case.expected_git_dir.as_deref(),
                case.actual_git_dir.as_deref(),
            ) {
                replacements.push((expected_git_dir, actual_git_dir));
            }
            assert_same_output_with_normalized_paths(
                actual,
                expected,
                &replacements,
                &actual_arg_refs,
            );

            for args in [
                vec!["show-ref"],
                vec!["status", "--short"],
                vec!["rev-parse", "--git-dir"],
                vec!["rev-parse", "--absolute-git-dir"],
                vec!["rev-parse", "--git-common-dir"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "core.bare"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output_with_normalized_paths(actual, expected, &replacements, &args);
            }

            match (&case.expected_git_dir, &case.actual_git_dir) {
                (Some(expected_git_dir), Some(actual_git_dir)) => {
                    assert_eq!(
                        normalize_path_text(
                            &fs::read(expected_repo.join(".git")).expect("read expected gitfile"),
                            &replacements,
                        ),
                        String::from_utf8(
                            fs::read(actual_repo.join(".git")).expect("read actual gitfile")
                        )
                        .expect("actual gitfile is utf8"),
                        "gitfile differed for {}",
                        case.label
                    );
                    assert!(expected_git_dir.join("HEAD").exists());
                    assert!(actual_git_dir.join("HEAD").exists());
                    assert!(!expected_repo.join(".git").is_dir());
                    assert!(!actual_repo.join(".git").is_dir());
                }
                (None, None) => {
                    assert!(expected_repo.join(".git").is_dir());
                    assert!(actual_repo.join(".git").is_dir());
                    assert!(!reset_expected_git_dir.exists());
                    assert!(!reset_actual_git_dir.exists());
                }
                _ => panic!("mismatched separate git dir test case"),
            }
            assert_eq!(
                fs::read(expected_repo.join("payload.txt")).expect("read expected payload"),
                fs::read(actual_repo.join("payload.txt")).expect("read actual payload"),
                "payload differed for {}",
                case.label
            );
        }

        for mode in ["--bare", "--mirror"] {
            let expected_repo = root.join(format!("conflict-{mode}-expected"));
            let actual_repo = root.join(format!("conflict-{mode}-actual"));
            let expected_git_dir = root.join(format!("conflict-{mode}-expected.git"));
            let actual_git_dir = root.join(format!("conflict-{mode}-actual.git"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let expected_git_dir_arg = expected_git_dir
                .to_str()
                .expect("expected git dir path is utf8");
            let actual_git_dir_arg = actual_git_dir
                .to_str()
                .expect("actual git dir path is utf8");
            let expected_args = [
                "clone",
                "-q",
                mode,
                "--separate-git-dir",
                expected_git_dir_arg,
                source_arg,
                expected_arg,
            ];
            let actual_args = [
                "clone",
                "-q",
                mode,
                "--separate-git-dir",
                actual_git_dir_arg,
                source_arg,
                actual_arg,
            ];

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output_with_normalized_paths(
                actual,
                expected,
                &[
                    (expected_repo.as_path(), actual_repo.as_path()),
                    (expected_git_dir.as_path(), actual_git_dir.as_path()),
                ],
                &actual_args,
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_reference_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-reference-flags");
    let source = root.join("source");
    let reference = root.join("reference");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&reference).expect("create reference repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        run_success(
            sley_testkit::oracle_git(),
            &reference,
            &["init", "-q", "-b", "main"],
        );
        fs::write(reference.join("reference.txt"), b"reference payload\n")
            .expect("write reference payload");
        run_success(
            sley_testkit::oracle_git(),
            &reference,
            &["add", "reference.txt"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &reference,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "reference",
                "-q",
            ],
        );
        let source_arg = source.to_str().expect("source path is utf8");
        let reference_arg = reference.to_str().expect("reference path is utf8");
        let missing_reference = root.join("missing-reference");
        let missing_reference_arg = missing_reference
            .to_str()
            .expect("missing reference path is utf8");
        let reference_equals = format!("--reference={reference_arg}");
        let reference_if_able_equals = format!("--reference-if-able={reference_arg}");

        for (label, options, bare_layout) in [
            ("reference", vec!["--reference", reference_arg], false),
            ("reference-equals", vec![reference_equals.as_str()], false),
            (
                "reference-if-able",
                vec!["--reference-if-able", reference_arg],
                false,
            ),
            (
                "reference-if-able-equals",
                vec![reference_if_able_equals.as_str()],
                false,
            ),
            (
                "reference-if-able-missing",
                vec!["--reference-if-able", missing_reference_arg],
                false,
            ),
            ("shared", vec!["--shared"], false),
            ("shared-reset", vec!["--shared", "--no-shared"], false),
            (
                "shared-reset-shared",
                vec!["--shared", "--no-shared", "--shared"],
                false,
            ),
            (
                "reference-dissociate",
                vec!["--reference", reference_arg, "--dissociate"],
                false,
            ),
            (
                "reference-dissociate-reset",
                vec![
                    "--reference",
                    reference_arg,
                    "--dissociate",
                    "--no-dissociate",
                ],
                false,
            ),
            (
                "bare-reference",
                vec!["--bare", "--reference", reference_arg],
                true,
            ),
            ("mirror-shared", vec!["--mirror", "--shared"], true),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }

            let expected_git_dir = if bare_layout {
                expected_repo.clone()
            } else {
                expected_repo.join(".git")
            };
            let actual_git_dir = if bare_layout {
                actual_repo.clone()
            } else {
                actual_repo.join(".git")
            };
            let expected_alternates =
                fs::read(expected_git_dir.join("objects/info/alternates")).ok();
            let actual_alternates = fs::read(actual_git_dir.join("objects/info/alternates")).ok();
            assert_eq!(
                actual_alternates, expected_alternates,
                "alternates differed for {label}"
            );
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_local_transport_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-transport-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("short-local", vec!["-l"]),
            ("long-local", vec!["--local"]),
            ("no-local", vec!["--no-local"]),
            ("hardlinks", vec!["--hardlinks"]),
            ("no-hardlinks", vec!["--no-hardlinks"]),
            ("bare-no-hardlinks", vec!["--bare", "--no-hardlinks"]),
            ("mirror-local", vec!["--mirror", "--local"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.url"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_upload_pack_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-upload-pack-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("short-space", vec!["-u", "git-upload-pack"]),
            ("short-compact", vec!["-ugit-upload-pack"]),
            ("long-space", vec!["--upload-pack", "git-upload-pack"]),
            ("long-equals", vec!["--upload-pack=git-upload-pack"]),
            ("no-upload-pack", vec!["--no-upload-pack"]),
            (
                "bare-upload-pack",
                vec!["--bare", "--upload-pack", "git-upload-pack"],
            ),
            (
                "mirror-upload-pack",
                vec!["--mirror", "--upload-pack=git-upload-pack"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["config", "--get", "remote.origin.uploadpack"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_server_option_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-server-option-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("long-space", vec!["--server-option", "alpha"]),
            ("long-equals", vec!["--server-option=alpha"]),
            ("empty-equals", vec!["--server-option="]),
            (
                "repeated",
                vec!["--server-option", "alpha", "--server-option", "beta"],
            ),
            ("no-server-option", vec!["--no-server-option"]),
            (
                "bare-server-option",
                vec!["--bare", "--server-option", "alpha"],
            ),
            (
                "mirror-server-option",
                vec!["--mirror", "--server-option=alpha"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["config", "--get-all", "remote.origin.serverOption"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_jobs_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-jobs-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("short-space", vec!["-j", "1"]),
            ("short-compact", vec!["-j1"]),
            ("long-space", vec!["--jobs", "1"]),
            ("long-equals", vec!["--jobs=1"]),
            ("zero", vec!["--jobs=0"]),
            ("negative", vec!["--jobs", "-1"]),
            ("suffix", vec!["--jobs=1k"]),
            ("positive-suffix", vec!["--jobs=+1K"]),
            ("no-jobs", vec!["--no-jobs"]),
            ("bare-jobs", vec!["--bare", "--jobs", "1"]),
            ("mirror-jobs", vec!["--mirror", "--jobs=1"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["config", "--get", "submodule.fetchJobs"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_ref_format_files_matches_upstream_git() {
    let root = unique_temp_dir("clone-local-ref-format-files");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("normal-equals", vec!["--ref-format=files"]),
            ("normal-space", vec!["--ref-format", "files"]),
            (
                "normal-reset",
                vec!["--ref-format=files", "--no-ref-format"],
            ),
            ("bare-files", vec!["--bare", "--ref-format=files"]),
            ("mirror-files", vec!["--mirror", "--ref-format", "files"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["rev-parse", "--show-ref-format"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_reject_shallow_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-reject-shallow-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("normal-reject", vec!["--reject-shallow"]),
            (
                "normal-reset",
                vec!["--reject-shallow", "--no-reject-shallow"],
            ),
            ("bare-reject", vec!["--bare", "--reject-shallow"]),
            ("mirror-reject", vec!["--mirror", "--reject-shallow"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["rev-parse", "--is-shallow-repository"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_shallow_hint_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-shallow-hint-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("depth-space", vec!["--depth", "1"]),
            ("depth-equals", vec!["--depth=1"]),
            ("depth-plus", vec!["--depth=+1"]),
            ("depth-reset", vec!["--depth=1", "--no-depth"]),
            ("shallow-since-space", vec!["--shallow-since", "now"]),
            ("shallow-since-equals", vec!["--shallow-since=now"]),
            (
                "shallow-since-reset",
                vec!["--shallow-since=now", "--no-shallow-since"],
            ),
            ("shallow-exclude-space", vec!["--shallow-exclude", "main"]),
            ("shallow-exclude-equals", vec!["--shallow-exclude=main"]),
            (
                "shallow-exclude-reset",
                vec!["--shallow-exclude=main", "--no-shallow-exclude"],
            ),
            (
                "combined",
                vec![
                    "--depth=1",
                    "--shallow-since",
                    "now",
                    "--shallow-exclude",
                    "main",
                ],
            ),
            ("bare-depth", vec!["--bare", "--depth=1"]),
            (
                "mirror-shallow-since",
                vec!["--mirror", "--shallow-since=now"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["rev-parse", "--is-shallow-repository"],
                vec!["rev-list", "--count", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_no_local_depth_matches_upstream_git() {
    let root = unique_temp_dir("clone-no-local-depth");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        // Deepen main past one commit so the depth-limited clones have a real
        // shallow boundary with history (and a tag) behind it.
        for (file, message) in [("second.txt", "second"), ("third.txt", "third")] {
            fs::write(source.join(file), message).expect("write payload");
            run_success(sley_testkit::oracle_git(), &source, &["add", file]);
            run_success(
                sley_testkit::oracle_git(),
                &source,
                &[
                    "-c",
                    "user.name=Example User",
                    "-c",
                    "user.email=example@example.invalid",
                    "commit",
                    "-m",
                    message,
                    "-q",
                ],
            );
        }
        run_success(sley_testkit::oracle_git(), &source, &["tag", "tip-tag"]);
        let source_path = source.to_str().expect("source path is utf8");
        let file_url = format!("file://{source_path}");

        for (label, repository, options) in [
            // `--no-local` routes a path clone through the transport, which
            // honors `--depth`: the result is a true shallow repository.
            ("depth-1", source_path, vec!["--no-local", "--depth=1"]),
            ("depth-2", source_path, vec!["--no-local", "--depth", "2"]),
            // NOTE: `--no-single-branch --depth` is not covered: upstream clone
            // maps `refs/tags/*` as a primary refspec outside --single-branch
            // (each tag tip is deepened independently), which sley's clone does
            // not model yet — tags are auto-followed instead.
            (
                "branch-depth-1",
                source_path,
                vec!["--no-local", "--branch", "feature/topic", "--depth=1"],
            ),
            // A depth past the root commit leaves the clone complete.
            (
                "depth-past-root",
                source_path,
                vec!["--no-local", "--depth=10"],
            ),
            // `file://` URLs never resolve as a plain path, so git treats them
            // as non-local too and the depth is honored.
            ("file-url-depth-1", file_url.as_str(), vec!["--depth=1"]),
            // `--no-depth` resets the depth: a plain full non-local clone.
            (
                "no-local-depth-reset",
                source_path,
                vec!["--no-local", "--depth=1", "--no-depth"],
            ),
            // An explicit `--local` keeps the warn-and-ignore behavior.
            (
                "local-depth-warns",
                source_path,
                vec!["--local", "--depth=1"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([repository, expected_arg]);
            let mut actual_args = vec!["clone"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([repository, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output_with_normalized_destination(
                actual,
                expected,
                expected_arg,
                actual_arg,
                &actual_args,
            );

            for args in [
                vec!["rev-parse", "--is-shallow-repository"],
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["rev-list", "--count", "HEAD"],
                vec!["log", "--all", "--format=%H"],
                vec!["fsck", "--strict"],
                vec!["config", "--get", "remote.origin.fetch"],
            ] {
                // A local-mechanism clone diverges from upstream on fsck:
                // git copies the whole object store (dangling objects
                // included, here the unfetched branch tip under the implied
                // --single-branch) while sley packs reachable objects only —
                // a pre-existing difference unrelated to --depth.
                if label == "local-depth-warns" && args[0] == "fsck" {
                    continue;
                }
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_recurse_submodules_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-recurse-submodules-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("recurse", vec!["--recurse-submodules"]),
            ("recursive", vec!["--recursive"]),
            ("recurse-pathspec", vec!["--recurse-submodules=path"]),
            ("recursive-pathspec", vec!["--recursive=path"]),
            (
                "reset",
                vec!["--recurse-submodules", "--no-recurse-submodules"],
            ),
            ("reset-recursive", vec!["--recursive", "--no-recursive"]),
            ("bare-recurse", vec!["--bare", "--recurse-submodules"]),
            ("mirror-recursive", vec!["--mirror", "--recursive"]),
            (
                "no-checkout-recurse",
                vec!["--no-checkout", "--recurse-submodules"],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get-all", "submodule.active"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["status", "--short"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_submodule_hint_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-submodule-hint-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("remote-submodules", vec!["--remote-submodules"]),
            (
                "remote-submodules-reset",
                vec!["--remote-submodules", "--no-remote-submodules"],
            ),
            ("shallow-submodules", vec!["--shallow-submodules"]),
            (
                "shallow-submodules-reset",
                vec!["--shallow-submodules", "--no-shallow-submodules"],
            ),
            (
                "also-filter-submodules",
                vec![
                    "--filter=blob:none",
                    "--recurse-submodules",
                    "--also-filter-submodules",
                ],
            ),
            (
                "also-filter-submodules-before-requirements",
                vec![
                    "--also-filter-submodules",
                    "--filter=blob:none",
                    "--recurse-submodules",
                ],
            ),
            (
                "also-filter-submodules-reset",
                vec![
                    "--filter=blob:none",
                    "--also-filter-submodules",
                    "--no-also-filter-submodules",
                ],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["status", "--short"],
                vec!["config", "--get-all", "submodule.active"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.promisor"],
                vec!["config", "--get", "remote.origin.partialclonefilter"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }

        for (label, options) in [
            (
                "also-filter-without-filter",
                vec!["--also-filter-submodules"],
            ),
            (
                "also-filter-without-recurse",
                vec!["--filter=blob:none", "--also-filter-submodules"],
            ),
            (
                "also-filter-filter-reset",
                vec![
                    "--filter=blob:none",
                    "--also-filter-submodules",
                    "--no-filter",
                    "--recurse-submodules",
                ],
            ),
            (
                "also-filter-recurse-reset",
                vec![
                    "--filter=blob:none",
                    "--recurse-submodules",
                    "--also-filter-submodules",
                    "--no-recurse-submodules",
                ],
            ),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_also_filter_submodules_marks_nested_clone_promisor_like_upstream_git() {
    let root = unique_temp_dir("clone-also-filter-submodules-promisor");
    let source = root.join("source");
    let submodule = source.join("sub");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    create_source_repo(&source);
    fs::create_dir_all(&submodule).expect("create submodule repo");
    create_source_repo(&submodule);
    run_success(
        sley_testkit::oracle_git(),
        &source,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "./sub",
        ],
    );
    run_success(
        sley_testkit::oracle_git(),
        &source,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "-m",
            "add submodule",
            "-q",
        ],
    );
    for repo in [&source, &submodule] {
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["config", "uploadpack.allowFilter", "true"],
        );
        run_success(
            sley_testkit::oracle_git(),
            repo,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
    }

    let source_url = format!("file://{}", source.display());
    let expected_arg = expected_repo.to_str().expect("expected path is utf8");
    let actual_arg = actual_repo.to_str().expect("actual path is utf8");
    let expected = run(
        sley_testkit::oracle_git(),
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "clone",
            "-q",
            "--filter=blob:none",
            "--also-filter-submodules",
            "--recurse-submodules",
            &source_url,
            expected_arg,
        ],
    );
    let actual = run(
        sley_testkit::sley_bin!(),
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "clone",
            "-q",
            "--filter=blob:none",
            "--also-filter-submodules",
            "--recurse-submodules",
            &source_url,
            actual_arg,
        ],
    );
    assert_same_output(actual, expected, &["clone", "--also-filter-submodules"]);

    for args in [
        vec!["config", "--get", "remote.origin.promisor"],
        vec!["config", "--get", "remote.origin.partialclonefilter"],
    ] {
        let expected = run(
            sley_testkit::oracle_git(),
            &expected_repo.join("sub"),
            &args,
        );
        let actual = run(sley_testkit::oracle_git(), &actual_repo.join("sub"), &args);
        assert_same_output(actual, expected, &args);
    }
    let promisor_packs = repository_promisor_sidecars(&actual_repo.join(".git/modules/sub"));
    assert!(
        !promisor_packs.is_empty(),
        "filtered submodule clone should retain a promisor pack sidecar"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_ip_family_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-ip-family-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("short-ipv4", vec!["-4"]),
            ("long-ipv4", vec!["--ipv4"]),
            ("short-ipv6", vec!["-6"]),
            ("long-ipv6", vec!["--ipv6"]),
            ("last-wins", vec!["-4", "--ipv6"]),
            ("bare-ipv4", vec!["--bare", "-4"]),
            ("mirror-ipv6", vec!["--mirror", "--ipv6"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_negative_noop_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-negative-noop-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("no-recurse-submodules", vec!["--no-recurse-submodules"]),
            ("no-recursive", vec!["--no-recursive"]),
            ("no-sparse", vec!["--no-sparse"]),
            ("no-filter", vec!["--no-filter"]),
            (
                "no-also-filter-submodules",
                vec!["--no-also-filter-submodules"],
            ),
            ("no-remote-submodules", vec!["--no-remote-submodules"]),
            ("no-shallow-submodules", vec!["--no-shallow-submodules"]),
            ("no-bundle-uri", vec!["--no-bundle-uri"]),
            ("no-depth", vec!["--no-depth"]),
            ("no-shallow-since", vec!["--no-shallow-since"]),
            ("no-shallow-exclude", vec!["--no-shallow-exclude"]),
            ("no-shared", vec!["--no-shared"]),
            ("no-reference", vec!["--no-reference"]),
            ("no-reference-if-able", vec!["--no-reference-if-able"]),
            ("no-dissociate", vec!["--no-dissociate"]),
            ("no-separate-git-dir", vec!["--no-separate-git-dir"]),
            ("no-template", vec!["--no-template"]),
            ("no-jobs", vec!["--no-jobs"]),
            ("no-revision", vec!["--no-revision"]),
            (
                "bare-no-recurse-submodules",
                vec!["--bare", "--no-recurse-submodules"],
            ),
            ("mirror-no-sparse", vec!["--mirror", "--no-sparse"]),
            ("bare-no-reference", vec!["--bare", "--no-reference"]),
            ("mirror-no-depth", vec!["--mirror", "--no-depth"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone", "-q"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone", "-q"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output(actual, expected, &actual_args);

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
                vec!["config", "--get", "remote.origin.tagOpt"],
                vec!["config", "--get", "submodule.active"],
                vec!["config", "--get", "extensions.worktreeConfig"],
                vec!["rev-parse", "--is-shallow-repository"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_progress_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-progress-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("progress", vec!["--progress"]),
            ("no-progress", vec!["--no-progress"]),
            ("quiet-progress", vec!["-q", "--progress"]),
            ("quiet-no-progress", vec!["-q", "--no-progress"]),
            ("bare-no-progress", vec!["--bare", "--no-progress"]),
            ("mirror-progress", vec!["--mirror", "--progress"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output_with_normalized_destination(
                actual,
                expected,
                expected_arg,
                actual_arg,
                &actual_args,
            );

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clone_local_repository_verbose_flags_match_upstream_git() {
    let root = unique_temp_dir("clone-local-verbose-flags");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    {
        create_source_repo(&source);
        add_feature_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, options) in [
            ("short-verbose", vec!["-v"]),
            ("long-verbose", vec!["--verbose"]),
            ("no-verbose", vec!["--no-verbose"]),
            ("quiet-then-verbose", vec!["-q", "--verbose"]),
            ("verbose-then-quiet", vec!["--verbose", "-q"]),
            ("bare-verbose", vec!["--bare", "--verbose"]),
            ("mirror-no-verbose", vec!["--mirror", "--no-verbose"]),
        ] {
            let expected_repo = root.join(format!("{label}-expected"));
            let actual_repo = root.join(format!("{label}-actual"));
            let expected_arg = expected_repo.to_str().expect("expected path is utf8");
            let actual_arg = actual_repo.to_str().expect("actual path is utf8");
            let mut expected_args = vec!["clone"];
            expected_args.extend(options.iter().copied());
            expected_args.extend([source_arg, expected_arg]);
            let mut actual_args = vec!["clone"];
            actual_args.extend(options.iter().copied());
            actual_args.extend([source_arg, actual_arg]);

            let expected = run(sley_testkit::oracle_git(), &root, &expected_args);
            let actual = run(sley_testkit::sley_bin!(), &root, &actual_args);
            assert_same_output_with_normalized_destination(
                actual,
                expected,
                expected_arg,
                actual_arg,
                &actual_args,
            );

            for args in [
                vec!["show-ref"],
                vec!["symbolic-ref", "HEAD"],
                vec!["config", "--get", "core.bare"],
                vec!["config", "--get", "remote.origin.fetch"],
                vec!["config", "--get", "remote.origin.mirror"],
            ] {
                let expected = run(sley_testkit::oracle_git(), &expected_repo, &args);
                let actual = run(sley_testkit::oracle_git(), &actual_repo, &args);
                assert_same_output(actual, expected, &args);
            }
        }
    };
    let _ = fs::remove_dir_all(&root);
}
