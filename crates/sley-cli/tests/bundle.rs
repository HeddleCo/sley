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

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => panic!("failed to read {}: {err}", path.display()),
    }
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
        "expected one index in {}",
        pack_dir.display()
    );
    assert_eq!(packs[0].file_stem(), indexes[0].file_stem());
    (packs.remove(0), indexes.remove(0))
}

fn repository_pack_indexes(git_dir: &Path) -> Vec<PathBuf> {
    let pack_dir = git_dir.join("objects").join("pack");
    let Ok(entries) = fs::read_dir(&pack_dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.expect("read pack entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
        .collect()
}

fn create_bundle_fixture(root: &Path) -> (PathBuf, String) {
    create_bundle_fixture_with_init(root, &["init", "-q"])
}

fn create_sha256_bundle_fixture(root: &Path) -> (PathBuf, String) {
    create_bundle_fixture_with_init(root, &["init", "-q", "--object-format=sha256"])
}

fn create_bundle_fixture_with_init(root: &Path, init_args: &[&str]) -> (PathBuf, String) {
    run_success("git", root, init_args);
    fs::write(root.join("payload.txt"), b"bundle payload\n").expect("write payload");
    run_success("git", root, &["add", "payload.txt"]);
    run_success(
        "git",
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
    run_success("git", root, &["branch", "feature/topic"]);
    run_success("git", root, &["tag", "v1.0"]);
    let head = String::from_utf8(run_success("git", root, &["rev-parse", "HEAD"]))
        .expect("HEAD is utf8")
        .trim()
        .to_string();
    let bundle = root.join("repo.bundle");
    run_success(
        "git",
        root,
        &[
            "bundle",
            "create",
            bundle.to_str().expect("bundle path is utf8"),
            "--all",
        ],
    );
    (bundle, head)
}

fn create_fetch_fixture_without_tags(root: &Path) -> String {
    run_success("git", root, &["init", "-q"]);
    fs::write(root.join("payload.txt"), b"fetch payload\n").expect("write payload");
    run_success("git", root, &["add", "payload.txt"]);
    run_success(
        "git",
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
    run_success("git", root, &["branch", "feature/topic"]);
    String::from_utf8(run_success("git", root, &["rev-parse", "HEAD"]))
        .expect("HEAD is utf8")
        .trim()
        .to_string()
}

fn add_tag_only_commit(root: &Path) {
    let current = String::from_utf8(run_success(
        "git",
        root,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    ))
    .expect("current branch is utf8")
    .trim()
    .to_string();
    run_success("git", root, &["checkout", "-q", "--detach", "HEAD"]);
    run_success(
        "git",
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "tag only",
            "-q",
        ],
    );
    run_success("git", root, &["tag", "tag-only"]);
    run_success("git", root, &["checkout", "-q", &current]);
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

fn percent_encoded_file_url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace(' ', "%20"))
}

fn ssh_url(path: &Path) -> String {
    format!("ssh://fake-host{}", path.to_string_lossy())
}

fn percent_encoded_ssh_url(path: &Path) -> String {
    format!(
        "ssh://fake-host{}",
        path.to_string_lossy().replace(' ', "%20")
    )
}

fn fake_ssh_script(root: &Path) -> PathBuf {
    let script = root.join("fake-ssh.sh");
    fs::write(
        &script,
        b"#!/bin/sh\nlast=''\nfor arg in \"$@\"; do last=$arg; done\neval \"exec $last\"\n",
    )
    .expect("write fake ssh script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).expect("stat fake ssh").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("chmod fake ssh");
    }
    script
}

fn create_incremental_bundle_fixture(root: &Path) -> PathBuf {
    run_success("git", root, &["init", "-q"]);
    run_success(
        "git",
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
            "-q",
        ],
    );
    let base = String::from_utf8(run_success("git", root, &["rev-parse", "HEAD"]))
        .expect("base is utf8")
        .trim()
        .to_string();
    run_success(
        "git",
        root,
        &[
            "-c",
            "user.name=Example User",
            "-c",
            "user.email=example@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "second",
            "-q",
        ],
    );
    let bundle = root.join("incremental.bundle");
    run_success(
        "git",
        root,
        &[
            "bundle",
            "create",
            bundle.to_str().expect("bundle path is utf8"),
            "HEAD",
            &format!("^{base}"),
        ],
    );
    bundle
}

#[test]
fn bundle_list_heads_matches_upstream_git() {
    let root = unique_temp_dir("bundle-list-heads");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (bundle, _) = create_bundle_fixture(&root);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        for args in [
            vec!["bundle", "list-heads", bundle],
            vec!["bundle", "list-heads", bundle, "refs/heads/feature/topic"],
            vec!["bundle", "list-heads", bundle, "feature/topic"],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_list_heads_matches_upstream_git_sha256() {
    let root = unique_temp_dir("bundle-list-heads-sha256");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (bundle, _) = create_sha256_bundle_fixture(&root);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        for args in [
            vec!["bundle", "list-heads", bundle],
            vec!["bundle", "list-heads", bundle, "refs/heads/feature/topic"],
            vec!["bundle", "list-heads", bundle, "feature/topic"],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_verify_matches_upstream_git() {
    let root = unique_temp_dir("bundle-verify");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (bundle, _) = create_bundle_fixture(&root);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        for args in [
            vec!["bundle", "verify", bundle],
            vec!["bundle", "verify", "-q", bundle],
            vec!["bundle", "verify", "--quiet", bundle],
        ] {
            let expected = run("git", &root, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
            assert_same_output(actual, expected, &args);
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_verify_prerequisites_match_upstream_git() {
    let root = unique_temp_dir("bundle-verify-prereq");
    let source = root.join("source");
    let expected_empty = root.join("expected-empty");
    let actual_empty = root.join("actual-empty");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_empty).expect("create expected repo");
    fs::create_dir_all(&actual_empty).expect("create actual repo");
    let result = (|| {
        let bundle = create_incremental_bundle_fixture(&source);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = ["bundle", "verify", bundle];
        let expected = run("git", &source, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &source, &args);
        assert_same_output(actual, expected, &args);

        run_success("git", &expected_empty, &["init", "-q"]);
        run_success("git", &actual_empty, &["init", "-q"]);
        let expected = run("git", &expected_empty, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_empty, &args);
        assert_same_output(actual, expected, &args);
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_create_all_writes_upstream_readable_bundle() {
    let root = unique_temp_dir("bundle-create-all");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (expected_bundle, head) = create_bundle_fixture(&root);
        let actual_bundle = root.join("actual.bundle");
        let actual_bundle_arg = actual_bundle.to_str().expect("bundle path is utf8");
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["bundle", "create", actual_bundle_arg, "--all"],
        );

        let expected_heads = run_success(
            "git",
            &root,
            &[
                "bundle",
                "list-heads",
                expected_bundle.to_str().expect("bundle path is utf8"),
            ],
        );
        let actual_heads = run_success("git", &root, &["bundle", "list-heads", actual_bundle_arg]);
        assert_eq!(actual_heads, expected_heads);

        run_success("git", &root, &["bundle", "verify", actual_bundle_arg]);
        let destination = root.join("destination");
        fs::create_dir_all(&destination).expect("create destination repo");
        run_success("git", &destination, &["init", "-q"]);
        run_success(
            "git",
            &destination,
            &["bundle", "unbundle", actual_bundle_arg],
        );
        let imported = run_success(
            "git",
            &destination,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_create_all_writes_upstream_readable_sha256_bundle() {
    let root = unique_temp_dir("bundle-create-all-sha256");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (expected_bundle, head) = create_sha256_bundle_fixture(&root);
        let actual_bundle = root.join("actual.bundle");
        let actual_bundle_arg = actual_bundle.to_str().expect("bundle path is utf8");
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["bundle", "create", actual_bundle_arg, "--all"],
        );

        let expected_heads = run_success(
            "git",
            &root,
            &[
                "bundle",
                "list-heads",
                expected_bundle.to_str().expect("bundle path is utf8"),
            ],
        );
        let actual_heads = run_success("git", &root, &["bundle", "list-heads", actual_bundle_arg]);
        assert_eq!(actual_heads, expected_heads);

        let verify_output = run_success("git", &root, &["bundle", "verify", actual_bundle_arg]);
        assert!(
            String::from_utf8_lossy(&verify_output).contains("hash algorithm: sha256"),
            "verify output should mention sha256, got {}",
            String::from_utf8_lossy(&verify_output)
        );

        let destination = root.join("destination");
        fs::create_dir_all(&destination).expect("create destination repo");
        run_success(
            "git",
            &destination,
            &["init", "-q", "--object-format=sha256"],
        );
        run_success(
            "git",
            &destination,
            &["bundle", "unbundle", actual_bundle_arg],
        );
        let imported = run_success(
            "git",
            &destination,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_create_all_with_explicit_revisions_matches_upstream() {
    let root = unique_temp_dir("bundle-create-all-explicit");
    fs::create_dir_all(&root).expect("create temp repo");
    let result = (|| {
        let (expected_all_bundle, head) = create_bundle_fixture(&root);
        let expected_bundle = root.join("expected-all-head.bundle");
        let actual_bundle = root.join("actual-all-head.bundle");
        let expected_bundle_arg = expected_bundle
            .to_str()
            .expect("expected bundle path is utf8");
        let actual_bundle_arg = actual_bundle.to_str().expect("actual bundle path is utf8");

        let args = ["bundle", "create", expected_bundle_arg, "--all", "HEAD"];
        let expected = run("git", &root, &args);
        let actual = run(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["bundle", "create", actual_bundle_arg, "--all", "HEAD"],
        );
        assert_same_output(actual, expected, &args);

        let all_heads = run_success(
            "git",
            &root,
            &[
                "bundle",
                "list-heads",
                expected_all_bundle
                    .to_str()
                    .expect("all bundle path is utf8"),
            ],
        );
        let actual_heads = run_success("git", &root, &["bundle", "list-heads", actual_bundle_arg]);
        assert_eq!(actual_heads, all_heads);
        run_success("git", &root, &["bundle", "verify", actual_bundle_arg]);

        let destination = root.join("destination-all-head");
        fs::create_dir_all(&destination).expect("create destination repo");
        run_success("git", &destination, &["init", "-q"]);
        run_success(
            "git",
            &destination,
            &["bundle", "unbundle", actual_bundle_arg],
        );
        let imported = run_success(
            "git",
            &destination,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");

        let expected_empty = root.join("expected-empty.bundle");
        let actual_empty = root.join("actual-empty.bundle");
        let expected_empty_arg = expected_empty
            .to_str()
            .expect("expected empty path is utf8");
        let actual_empty_arg = actual_empty.to_str().expect("actual empty path is utf8");
        let expected = run(
            "git",
            &root,
            &["bundle", "create", expected_empty_arg, "--all", "^HEAD"],
        );
        let actual = run(
            env!("CARGO_BIN_EXE_sley"),
            &root,
            &["bundle", "create", actual_empty_arg, "--all", "^HEAD"],
        );
        assert_same_output(
            actual,
            expected,
            &["bundle", "create", "<bundle>", "--all", "^HEAD"],
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_create_incremental_writes_upstream_readable_bundle() {
    let root = unique_temp_dir("bundle-create-incremental");
    let source = root.join("source");
    let destination = root.join("destination");
    let expected_empty = root.join("expected-empty");
    let actual_empty = root.join("actual-empty");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_empty).expect("create expected empty repo");
    fs::create_dir_all(&actual_empty).expect("create actual empty repo");
    let result = (|| {
        run_success("git", &source, &["init", "-q"]);
        fs::write(source.join("payload.txt"), b"base payload\n").expect("write base payload");
        run_success("git", &source, &["add", "payload.txt"]);
        run_success(
            "git",
            &source,
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
        let base = String::from_utf8(run_success("git", &source, &["rev-parse", "HEAD"]))
            .expect("base is utf8")
            .trim()
            .to_string();
        run_success(
            "git",
            &root,
            &[
                "clone",
                "-q",
                source.to_str().expect("source path is utf8"),
                destination.to_str().expect("destination path is utf8"),
            ],
        );
        fs::write(source.join("payload.txt"), b"changed payload\n").expect("write changed payload");
        run_success("git", &source, &["add", "payload.txt"]);
        run_success(
            "git",
            &source,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second",
                "-q",
            ],
        );
        let head = String::from_utf8(run_success("git", &source, &["rev-parse", "HEAD"]))
            .expect("HEAD is utf8")
            .trim()
            .to_string();
        let expected_bundle = source.join("expected-incremental.bundle");
        let actual_bundle = source.join("actual-incremental.bundle");
        let expected_bundle_arg = expected_bundle.to_str().expect("bundle path is utf8");
        let actual_bundle_arg = actual_bundle.to_str().expect("bundle path is utf8");
        run_success(
            "git",
            &source,
            &[
                "bundle",
                "create",
                expected_bundle_arg,
                "HEAD",
                &format!("^{base}"),
            ],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &source,
            &[
                "bundle",
                "create",
                actual_bundle_arg,
                "HEAD",
                &format!("^{base}"),
            ],
        );

        let expected_heads = run_success(
            "git",
            &source,
            &["bundle", "list-heads", expected_bundle_arg],
        );
        let actual_heads =
            run_success("git", &source, &["bundle", "list-heads", actual_bundle_arg]);
        assert_eq!(actual_heads, expected_heads);

        run_success("git", &source, &["bundle", "verify", actual_bundle_arg]);
        run_success("git", &expected_empty, &["init", "-q"]);
        run_success("git", &actual_empty, &["init", "-q"]);
        let expected_missing = run(
            "git",
            &expected_empty,
            &["bundle", "verify", expected_bundle_arg],
        );
        let actual_missing = run(
            "git",
            &actual_empty,
            &["bundle", "verify", actual_bundle_arg],
        );
        assert_same_output(
            actual_missing,
            expected_missing,
            &["bundle", "verify", "<bundle>"],
        );

        run_success(
            "git",
            &destination,
            &["bundle", "unbundle", actual_bundle_arg],
        );
        let imported = run_success(
            "git",
            &destination,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"changed payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_bundle_refspec_matches_upstream_git() {
    let root = unique_temp_dir("bundle-fetch-refspec");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = [
            "fetch",
            "-q",
            bundle,
            "refs/heads/feature/topic:refs/heads/imported",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);

        let expected_ref = run_success("git", &expected_repo, &["show-ref", "refs/heads/imported"]);
        let actual_ref = run_success("git", &actual_repo, &["show-ref", "refs/heads/imported"]);
        assert_eq!(actual_ref, expected_ref);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_bundle_no_tags_disables_auto_follow_like_upstream_git() {
    let root = unique_temp_dir("bundle-fetch-no-tags");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = [
            "fetch",
            "-q",
            "--no-tags",
            bundle,
            "refs/heads/feature/topic:refs/heads/imported",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let expected_tag = run("git", &expected_repo, &["show-ref", "refs/tags/v1.0"]);
        let actual_tag = run("git", &actual_repo, &["show-ref", "refs/tags/v1.0"]);
        assert_same_output(actual_tag, expected_tag, &["show-ref", "refs/tags/v1.0"]);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_bundle_tags_fetches_unfollowed_tags_like_upstream_git() {
    let root = unique_temp_dir("bundle-fetch-tags");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        add_tag_only_commit(&source);
        fs::remove_file(&bundle).expect("remove old bundle");
        run_success(
            "git",
            &source,
            &[
                "bundle",
                "create",
                bundle.to_str().expect("bundle path is utf8"),
                "--all",
            ],
        );
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = [
            "fetch",
            "-q",
            "--tags",
            bundle,
            "refs/heads/feature/topic:refs/heads/imported",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        for reference in ["refs/heads/imported", "refs/tags/tag-only"] {
            let expected_ref = run_success("git", &expected_repo, &["show-ref", reference]);
            let actual_ref = run_success("git", &actual_repo, &["show-ref", reference]);
            assert_eq!(actual_ref, expected_ref, "ref {reference} differed");
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_bundle_source_ref_writes_fetch_head_without_ref_update() {
    let root = unique_temp_dir("bundle-fetch-fetch-head");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = ["fetch", "-q", bundle, "refs/heads/feature/topic"];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        assert_eq!(
            run("git", &actual_repo, &["show-ref"]).status.code(),
            Some(1)
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_bundle_default_head_writes_fetch_head_without_ref_update() {
    let root = unique_temp_dir("bundle-fetch-default-head");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");
        let args = ["fetch", "-q", bundle];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        assert_eq!(
            run("git", &actual_repo, &["show-ref"]).status.code(),
            Some(1)
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_refspec_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-refspec");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source = source.to_str().expect("source path is utf8");
        let args = [
            "fetch",
            "-q",
            source,
            "refs/heads/feature/topic:refs/remotes/origin/feature/topic",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);

        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_ssh_repository_refspec_matches_upstream_git_protocol_v0() {
    let root = unique_temp_dir("ssh-fetch-refspec");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let head = create_fetch_fixture_without_tags(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let fake_ssh = fake_ssh_script(&root);
        let fake_ssh = fake_ssh.to_str().expect("fake ssh path is utf8");
        let remote_url = ssh_url(&source);
        let args = [
            "fetch",
            "-q",
            "--no-tags",
            remote_url.as_str(),
            "refs/heads/feature/topic:refs/remotes/ssh/feature/topic",
        ];
        let expected_args = [
            "-c",
            "protocol.version=0",
            "fetch",
            "-q",
            "--no-tags",
            remote_url.as_str(),
            "refs/heads/feature/topic:refs/remotes/ssh/feature/topic",
        ];

        let expected = run_with_env(
            "git",
            &expected_repo,
            &expected_args,
            &[("GIT_SSH", fake_ssh)],
        );
        let actual = run_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &args,
            &[("GIT_SSH", fake_ssh)],
        );
        assert_same_output(actual, expected, &args);

        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/ssh/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/ssh/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"fetch payload\n");
        assert!(
            !repository_pack_indexes(&actual_repo.join(".git")).is_empty(),
            "SSH fetch should install a pack"
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_percent_encoded_ssh_remote_matches_upstream_git_protocol_v0() {
    let root = unique_temp_dir("ssh-fetch-configured-percent");
    let source = root.join("source with space");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let head = create_fetch_fixture_without_tags(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let fake_ssh = fake_ssh_script(&root);
        let fake_ssh = fake_ssh.to_str().expect("fake ssh path is utf8");
        let remote_url = percent_encoded_ssh_url(&source);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", remote_url.as_str()],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", remote_url.as_str()],
        );
        let args = ["fetch", "-q", "--no-tags", "origin"];
        let expected_args = [
            "-c",
            "protocol.version=0",
            "fetch",
            "-q",
            "--no-tags",
            "origin",
        ];

        let expected = run_with_env(
            "git",
            &expected_repo,
            &expected_args,
            &[("GIT_SSH", fake_ssh)],
        );
        let actual = run_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &args,
            &[("GIT_SSH", fake_ssh)],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"fetch payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_ssh_remote_prune_matches_upstream_git_protocol_v0() {
    let root = unique_temp_dir("ssh-fetch-configured-prune");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let head = create_fetch_fixture_without_tags(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let fake_ssh = fake_ssh_script(&root);
        let fake_ssh = fake_ssh.to_str().expect("fake ssh path is utf8");
        let remote_url = ssh_url(&source);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", remote_url.as_str()],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", remote_url.as_str()],
        );
        let initial_args = ["fetch", "-q", "--no-tags", "origin"];
        let expected_initial_args = [
            "-c",
            "protocol.version=0",
            "fetch",
            "-q",
            "--no-tags",
            "origin",
        ];
        run_success_with_env(
            "git",
            &expected_repo,
            &expected_initial_args,
            &[("GIT_SSH", fake_ssh)],
        );
        run_success_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &initial_args,
            &[("GIT_SSH", fake_ssh)],
        );
        run_success("git", &source, &["branch", "-D", "feature/topic"]);

        let args = ["fetch", "-q", "--prune", "--no-tags", "origin"];
        let expected_args = [
            "-c",
            "protocol.version=0",
            "fetch",
            "-q",
            "--prune",
            "--no-tags",
            "origin",
        ];
        let expected = run_with_env(
            "git",
            &expected_repo,
            &expected_args,
            &[("GIT_SSH", fake_ssh)],
        );
        let actual = run_with_env(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &args,
            &[("GIT_SSH", fake_ssh)],
        );
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        for reference in [
            "refs/remotes/origin/feature/topic",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
        ] {
            let expected_ref = run("git", &expected_repo, &["show-ref", reference]);
            let actual_ref = run("git", &actual_repo, &["show-ref", reference]);
            assert_same_output(actual_ref, expected_ref, &["show-ref", reference]);
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"fetch payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_no_tags_disables_auto_follow_like_upstream_git() {
    let root = unique_temp_dir("local-fetch-no-tags");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source = source.to_str().expect("source path is utf8");
        let args = [
            "fetch",
            "-q",
            "--no-tags",
            source,
            "refs/heads/feature/topic:refs/remotes/origin/feature/topic",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let expected_tag = run("git", &expected_repo, &["show-ref", "refs/tags/v1.0"]);
        let actual_tag = run("git", &actual_repo, &["show-ref", "refs/tags/v1.0"]);
        assert_same_output(actual_tag, expected_tag, &["show-ref", "refs/tags/v1.0"]);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_tags_fetches_unfollowed_tags_like_upstream_git() {
    let root = unique_temp_dir("local-fetch-tags");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        add_tag_only_commit(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source = source.to_str().expect("source path is utf8");
        let args = [
            "fetch",
            "-q",
            "--tags",
            source,
            "refs/heads/feature/topic:refs/remotes/origin/feature/topic",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        for reference in ["refs/remotes/origin/feature/topic", "refs/tags/tag-only"] {
            let expected_ref = run_success("git", &expected_repo, &["show-ref", reference]);
            let actual_ref = run_success("git", &actual_repo, &["show-ref", reference]);
            assert_eq!(actual_ref, expected_ref, "ref {reference} differed");
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_default_head_writes_fetch_head_without_ref_update() {
    let root = unique_temp_dir("local-fetch-default-head");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source = source.to_str().expect("source path is utf8");
        let args = ["fetch", "-q", source];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        assert_eq!(
            run("git", &actual_repo, &["show-ref"]).status.code(),
            Some(1)
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let head_branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("head branch is utf8")
        .trim()
        .to_string();
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, args) in [
            ("explicit-origin", vec!["fetch", "-q", "origin"]),
            ("default-origin", vec!["fetch", "-q"]),
        ] {
            let expected_repo = root.join(format!("expected-{label}"));
            let actual_repo = root.join(format!("actual-{label}"));
            fs::create_dir_all(&expected_repo).expect("create expected repo");
            fs::create_dir_all(&actual_repo).expect("create actual repo");
            run_success("git", &expected_repo, &["init", "-q"]);
            run_success("git", &actual_repo, &["init", "-q"]);
            run_success(
                "git",
                &expected_repo,
                &["remote", "add", "origin", source_arg],
            );
            run_success(
                "git",
                &actual_repo,
                &["remote", "add", "origin", source_arg],
            );

            let expected = run("git", &expected_repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read expected FETCH_HEAD"),
                fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read actual FETCH_HEAD"),
                "FETCH_HEAD differed for {args:?}"
            );

            for reference in [
                "refs/remotes/origin/feature/topic".to_string(),
                format!("refs/remotes/origin/{head_branch}"),
            ] {
                let expected_ref = run_success("git", &expected_repo, &["show-ref", &reference]);
                let actual_ref = run_success("git", &actual_repo, &["show-ref", &reference]);
                assert_eq!(actual_ref, expected_ref, "ref {reference} differed");
            }
            let imported = run_success(
                "git",
                &actual_repo,
                &["cat-file", "-p", &format!("{head}:payload.txt")],
            );
            assert_eq!(imported, b"bundle payload\n");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_tagopt_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-tagopt");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        add_tag_only_commit(&source);
        let source_arg = source.to_str().expect("source path is utf8");

        for (label, remote_args, fetch_args, expected_tag_present) in [
            (
                "no-tags",
                vec!["remote", "add", "--no-tags", "origin", source_arg],
                vec!["fetch", "-q", "origin"],
                false,
            ),
            (
                "tags",
                vec!["remote", "add", "--tags", "origin", source_arg],
                vec!["fetch", "-q", "origin"],
                true,
            ),
            (
                "cli-overrides-no-tags",
                vec!["remote", "add", "--no-tags", "origin", source_arg],
                vec!["fetch", "-q", "--tags", "origin"],
                true,
            ),
        ] {
            let expected_repo = root.join(format!("expected-{label}"));
            let actual_repo = root.join(format!("actual-{label}"));
            fs::create_dir_all(&expected_repo).expect("create expected repo");
            fs::create_dir_all(&actual_repo).expect("create actual repo");
            run_success("git", &expected_repo, &["init", "-q"]);
            run_success("git", &actual_repo, &["init", "-q"]);
            run_success("git", &expected_repo, &remote_args);
            run_success("git", &actual_repo, &remote_args);

            let expected = run("git", &expected_repo, &fetch_args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &fetch_args);
            assert_same_output(actual, expected, &fetch_args);
            assert_eq!(
                fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read expected FETCH_HEAD"),
                fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read actual FETCH_HEAD"),
                "FETCH_HEAD differed for {fetch_args:?}"
            );
            let expected_ref = run_success(
                "git",
                &expected_repo,
                &["show-ref", "refs/remotes/origin/feature/topic"],
            );
            let actual_ref = run_success(
                "git",
                &actual_repo,
                &["show-ref", "refs/remotes/origin/feature/topic"],
            );
            assert_eq!(actual_ref, expected_ref);

            let expected_tag = run("git", &expected_repo, &["show-ref", "refs/tags/tag-only"]);
            let actual_tag = run("git", &actual_repo, &["show-ref", "refs/tags/tag-only"]);
            let actual_tag_present = actual_tag.status.success();
            assert_same_output(
                actual_tag,
                expected_tag,
                &["show-ref", "refs/tags/tag-only"],
            );
            assert_eq!(
                actual_tag_present, expected_tag_present,
                "tag-only presence differed for {label}"
            );
            let imported = run_success(
                "git",
                &actual_repo,
                &["cat-file", "-p", &format!("{head}:payload.txt")],
            );
            assert_eq!(imported, b"bundle payload\n");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_prune_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-prune");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let head_branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("head branch is utf8")
        .trim()
        .to_string();
        let source_arg = source.to_str().expect("source path is utf8");
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", source_arg],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", source_arg],
        );
        run_success("git", &expected_repo, &["fetch", "-q", "origin"]);
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &["fetch", "-q", "origin"],
        );
        run_success("git", &source, &["branch", "-D", "feature/topic"]);

        let dry_run_args = ["fetch", "-q", "--dry-run", "--prune", "origin"];
        let expected = run("git", &expected_repo, &dry_run_args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &dry_run_args);
        assert_same_output(actual, expected, &dry_run_args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD after dry-run prune"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                .expect("read actual FETCH_HEAD after dry-run prune")
        );
        let stale_ref = "refs/remotes/origin/feature/topic";
        let expected_ref = run_success("git", &expected_repo, &["show-ref", stale_ref]);
        let actual_ref = run_success("git", &actual_repo, &["show-ref", stale_ref]);
        assert_eq!(actual_ref, expected_ref, "dry-run pruned {stale_ref}");

        let args = ["fetch", "-q", "--prune", "origin"];
        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        for reference in [
            "refs/remotes/origin/feature/topic".to_string(),
            format!("refs/remotes/origin/{head_branch}"),
        ] {
            let expected_ref = run("git", &expected_repo, &["show-ref", &reference]);
            let actual_ref = run("git", &actual_repo, &["show-ref", &reference]);
            assert_same_output(actual_ref, expected_ref, &["show-ref", &reference]);
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_prune_config_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-prune-config");
    fs::create_dir_all(&root).expect("create root");
    let result = (|| {
        let run_case =
            |label: &str, config_args: Vec<Vec<&str>>, fetch_args: Vec<&str>, stale_kept: bool| {
                let source = root.join(format!("{label}-source"));
                let expected_repo = root.join(format!("{label}-expected"));
                let actual_repo = root.join(format!("{label}-actual"));
                fs::create_dir_all(&source).expect("create source repo");
                fs::create_dir_all(&expected_repo).expect("create expected repo");
                fs::create_dir_all(&actual_repo).expect("create actual repo");
                let (_bundle, _head) = create_bundle_fixture(&source);
                let head_branch = String::from_utf8(run_success(
                    "git",
                    &source,
                    &["rev-parse", "--abbrev-ref", "HEAD"],
                ))
                .expect("head branch is utf8")
                .trim()
                .to_string();
                let source_arg = source.to_str().expect("source path is utf8");
                run_success("git", &expected_repo, &["init", "-q"]);
                run_success("git", &actual_repo, &["init", "-q"]);
                run_success(
                    "git",
                    &expected_repo,
                    &["remote", "add", "origin", source_arg],
                );
                run_success(
                    "git",
                    &actual_repo,
                    &["remote", "add", "origin", source_arg],
                );
                run_success("git", &expected_repo, &["fetch", "-q", "origin"]);
                run_success(
                    env!("CARGO_BIN_EXE_sley"),
                    &actual_repo,
                    &["fetch", "-q", "origin"],
                );
                run_success("git", &source, &["branch", "-D", "feature/topic"]);
                for args in &config_args {
                    run_success("git", &expected_repo, args);
                    run_success("git", &actual_repo, args);
                }

                let expected = run("git", &expected_repo, &fetch_args);
                let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &fetch_args);
                assert_same_output(actual, expected, &fetch_args);
                assert_eq!(
                    fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                        .expect("read expected FETCH_HEAD"),
                    fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                        .expect("read actual FETCH_HEAD"),
                    "FETCH_HEAD differed for {label}"
                );
                for reference in [
                    "refs/remotes/origin/feature/topic".to_string(),
                    format!("refs/remotes/origin/{head_branch}"),
                ] {
                    let expected_ref = run("git", &expected_repo, &["show-ref", &reference]);
                    let actual_ref = run("git", &actual_repo, &["show-ref", &reference]);
                    assert_same_output(actual_ref, expected_ref, &["show-ref", &reference]);
                }
                let expected_stale = run(
                    "git",
                    &expected_repo,
                    &["show-ref", "refs/remotes/origin/feature/topic"],
                );
                assert_eq!(
                    expected_stale.status.success(),
                    stale_kept,
                    "stale ref expectation differed for {label}"
                );
            };

        run_case(
            "remote-true",
            vec![vec!["config", "remote.origin.prune", "true"]],
            vec!["fetch", "-q", "origin"],
            false,
        );
        run_case(
            "fetch-true",
            vec![vec!["config", "fetch.prune", "true"]],
            vec!["fetch", "-q", "origin"],
            false,
        );
        run_case(
            "no-prune-overrides-remote-true",
            vec![vec!["config", "remote.origin.prune", "true"]],
            vec!["fetch", "-q", "--no-prune", "origin"],
            true,
        );
        run_case(
            "remote-false-overrides-fetch-true",
            vec![
                vec!["config", "fetch.prune", "true"],
                vec!["config", "remote.origin.prune", "false"],
            ],
            vec!["fetch", "-q", "origin"],
            true,
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_dry_run_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-dry-run");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let head_branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("head branch is utf8")
        .trim()
        .to_string();
        let source_arg = source.to_str().expect("source path is utf8");
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", source_arg],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", source_arg],
        );

        let dry_run_args = ["fetch", "-q", "--dry-run", "origin"];
        let expected = run("git", &expected_repo, &dry_run_args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &dry_run_args);
        assert_same_output(actual, expected, &dry_run_args);
        assert_eq!(
            read_optional(&expected_repo.join(".git").join("FETCH_HEAD")),
            read_optional(&actual_repo.join(".git").join("FETCH_HEAD")),
            "FETCH_HEAD differed after dry-run"
        );
        for args in [
            vec!["show-ref", "refs/remotes/origin/feature/topic"],
            vec!["cat-file", "-e", &head],
        ] {
            let expected = run("git", &expected_repo, &args);
            let actual = run("git", &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }

        let fetch_args = ["fetch", "-q", "--dry-run", "--no-dry-run", "origin"];
        let expected = run("git", &expected_repo, &fetch_args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &fetch_args);
        assert_same_output(actual, expected, &fetch_args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        for reference in [
            "refs/remotes/origin/feature/topic".to_string(),
            format!("refs/remotes/origin/{head_branch}"),
        ] {
            let expected_ref = run_success("git", &expected_repo, &["show-ref", &reference]);
            let actual_ref = run_success("git", &actual_repo, &["show-ref", &reference]);
            assert_eq!(actual_ref, expected_ref, "ref {reference} differed");
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_append_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-append");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let source_arg = source.to_str().expect("source path is utf8");
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", source_arg],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", source_arg],
        );

        for args in [
            vec!["fetch", "-q", "origin"],
            vec!["fetch", "-q", "--append", "origin"],
            vec!["fetch", "-q", "--append", "--no-append", "origin"],
        ] {
            let expected = run("git", &expected_repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read expected FETCH_HEAD"),
                fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read actual FETCH_HEAD"),
                "FETCH_HEAD differed for {args:?}"
            );
        }

        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_local_remote_no_write_fetch_head_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-no-write-fetch-head");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let head_branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("head branch is utf8")
        .trim()
        .to_string();
        let source_arg = source.to_str().expect("source path is utf8");
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", source_arg],
        );
        run_success(
            "git",
            &actual_repo,
            &["remote", "add", "origin", source_arg],
        );

        let args = ["fetch", "-q", "--no-write-fetch-head", "origin"];
        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            read_optional(&expected_repo.join(".git").join("FETCH_HEAD")),
            read_optional(&actual_repo.join(".git").join("FETCH_HEAD")),
            "FETCH_HEAD differed after --no-write-fetch-head"
        );
        for reference in [
            "refs/remotes/origin/feature/topic".to_string(),
            format!("refs/remotes/origin/{head_branch}"),
        ] {
            let expected_ref = run_success("git", &expected_repo, &["show-ref", &reference]);
            let actual_ref = run_success("git", &actual_repo, &["show-ref", &reference]);
            assert_eq!(actual_ref, expected_ref, "ref {reference} differed");
        }
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");

        let args = [
            "fetch",
            "-q",
            "--no-write-fetch-head",
            "--write-fetch-head",
            "origin",
        ];
        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_linked_worktree_head_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-linked-worktree-head");
    let source = root.join("source");
    let linked = root.join("linked");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        run_success("git", &source, &["init", "-q"]);
        fs::write(source.join("main.txt"), b"main payload\n").expect("write main payload");
        run_success("git", &source, &["add", "main.txt"]);
        run_success(
            "git",
            &source,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "main",
                "-q",
            ],
        );
        let default_branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("default branch is utf8")
        .trim()
        .to_string();
        run_success("git", &source, &["checkout", "-q", "-b", "feature"]);
        fs::write(source.join("feature.txt"), b"feature payload\n").expect("write feature payload");
        run_success("git", &source, &["add", "feature.txt"]);
        run_success(
            "git",
            &source,
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
        let feature_head = String::from_utf8(run_success("git", &source, &["rev-parse", "HEAD"]))
            .expect("feature head is utf8")
            .trim()
            .to_string();
        run_success("git", &source, &["checkout", "-q", &default_branch]);
        let linked_arg = linked.to_str().expect("linked path is utf8");
        run_success(
            "git",
            &source,
            &["worktree", "add", "-q", linked_arg, "feature"],
        );
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);

        let args = ["fetch", "-q", linked_arg];
        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{feature_head}:feature.txt")],
        );
        assert_eq!(imported, b"feature payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_file_url_and_rewritten_url_match_upstream_git() {
    let root = unique_temp_dir("local-fetch-file-url");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        let source_file_url = file_url(&source);
        let root_file_url = file_url(&root);

        for (label, setup, args) in [
            (
                "direct-file-url",
                Vec::<Vec<String>>::new(),
                vec![
                    "fetch".to_string(),
                    "-q".to_string(),
                    source_file_url.clone(),
                    "refs/heads/feature/topic:refs/remotes/file/feature/topic".to_string(),
                ],
            ),
            (
                "rewritten-url",
                vec![vec![
                    "config".to_string(),
                    format!("url.{root_file_url}/.insteadOf"),
                    "alias/".to_string(),
                ]],
                vec![
                    "fetch".to_string(),
                    "-q".to_string(),
                    "alias/source".to_string(),
                    "refs/heads/feature/topic:refs/remotes/file/feature/topic".to_string(),
                ],
            ),
        ] {
            let expected_repo = root.join(format!("expected-{label}"));
            let actual_repo = root.join(format!("actual-{label}"));
            fs::create_dir_all(&expected_repo).expect("create expected repo");
            fs::create_dir_all(&actual_repo).expect("create actual repo");
            run_success("git", &expected_repo, &["init", "-q"]);
            run_success("git", &actual_repo, &["init", "-q"]);
            for command in &setup {
                let command = command.iter().map(String::as_str).collect::<Vec<_>>();
                run_success("git", &expected_repo, &command);
                run_success("git", &actual_repo, &command);
            }

            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            let expected = run("git", &expected_repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
            assert_eq!(
                fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read expected FETCH_HEAD"),
                fs::read(actual_repo.join(".git").join("FETCH_HEAD"))
                    .expect("read actual FETCH_HEAD"),
                "FETCH_HEAD differed for {args:?}"
            );
            let expected_ref = run_success(
                "git",
                &expected_repo,
                &["show-ref", "refs/remotes/file/feature/topic"],
            );
            let actual_ref = run_success(
                "git",
                &actual_repo,
                &["show-ref", "refs/remotes/file/feature/topic"],
            );
            assert_eq!(actual_ref, expected_ref);
            let imported = run_success(
                "git",
                &actual_repo,
                &["cat-file", "-p", &format!("{head}:payload.txt")],
            );
            assert_eq!(imported, b"bundle payload\n");
        }
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_file_url_percent_encoded_path_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-file-url-percent-encoded");
    let source = root.join("source repo");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source_file_url = percent_encoded_file_url(&source);
        let args = [
            "fetch",
            "-q",
            &source_file_url,
            "refs/heads/feature/topic:refs/remotes/file/feature/topic",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/file/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/file/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_configured_file_url_percent_encoded_path_matches_upstream_git() {
    let root = unique_temp_dir("local-fetch-configured-file-url-percent-encoded");
    let source = root.join("source repo");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let source_file_url = percent_encoded_file_url(&source);
        run_success(
            "git",
            &expected_repo,
            &["remote", "add", "origin", source_file_url.as_str()],
        );
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &["remote", "add", "origin", source_file_url.as_str()],
        );
        let args = [
            "fetch",
            "-q",
            "origin",
            "refs/heads/feature/topic:refs/remotes/origin/feature/topic",
        ];

        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        assert_eq!(
            fs::read(expected_repo.join(".git").join("FETCH_HEAD"))
                .expect("read expected FETCH_HEAD"),
            fs::read(actual_repo.join(".git").join("FETCH_HEAD")).expect("read actual FETCH_HEAD")
        );
        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/origin/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_sha256_file_url_imports_objects_like_upstream_git() {
    let root = unique_temp_dir("local-fetch-sha256-file-url");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, head) = create_sha256_bundle_fixture(&source);
        let source_file_url = file_url(&source);
        run_success(
            "git",
            &expected_repo,
            &["init", "-q", "--object-format=sha256"],
        );
        run_success(
            "git",
            &actual_repo,
            &["init", "-q", "--object-format=sha256"],
        );
        let args = [
            "fetch",
            "-q",
            &source_file_url,
            "refs/heads/feature/topic:refs/remotes/file/feature/topic",
        ];
        let expected = run("git", &expected_repo, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
        assert_same_output(actual, expected, &args);
        let actual_git_dir = actual_repo.join(".git");
        let (_pack_path, index_path) = repository_pack_pair(&actual_git_dir);
        let index_arg = index_path.to_string_lossy();
        run_success("git", &actual_repo, &["verify-pack", "-v", &index_arg]);
        assert!(
            !loose_object_path(&actual_git_dir, &head).exists(),
            "fetched commit should be stored in pack, not as loose object"
        );
        let expected_ref = run_success(
            "git",
            &expected_repo,
            &["show-ref", "refs/remotes/file/feature/topic"],
        );
        let actual_ref = run_success(
            "git",
            &actual_repo,
            &["show-ref", "refs/remotes/file/feature/topic"],
        );
        assert_eq!(actual_ref, expected_ref);
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"bundle payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn fetch_local_repository_protocol_pack_excludes_existing_haves() {
    let root = unique_temp_dir("local-fetch-upload-pack-haves");
    let source = root.join("source");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (_bundle, base) = create_bundle_fixture(&source);
        let branch = String::from_utf8(run_success(
            "git",
            &source,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ))
        .expect("branch is utf8")
        .trim()
        .to_string();
        let source_arg = source.to_str().expect("source path is utf8");
        run_success("git", &actual_repo, &["init", "-q"]);
        let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &["fetch", "-q", source_arg, &refspec],
        );
        let actual_git_dir = actual_repo.join(".git");
        let before_indexes = repository_pack_indexes(&actual_git_dir)
            .into_iter()
            .collect::<std::collections::HashSet<_>>();

        fs::write(source.join("payload.txt"), b"changed payload\n").expect("write changed");
        run_success("git", &source, &["add", "payload.txt"]);
        run_success(
            "git",
            &source,
            &[
                "-c",
                "user.name=Example User",
                "-c",
                "user.email=example@example.invalid",
                "commit",
                "-m",
                "second",
                "-q",
            ],
        );
        let head = String::from_utf8(run_success("git", &source, &["rev-parse", "HEAD"]))
            .expect("head is utf8")
            .trim()
            .to_string();
        run_success(
            env!("CARGO_BIN_EXE_sley"),
            &actual_repo,
            &["fetch", "-q", source_arg, &refspec],
        );

        let after_indexes = repository_pack_indexes(&actual_git_dir);
        let new_indexes = after_indexes
            .iter()
            .filter(|path| !before_indexes.contains(*path))
            .collect::<Vec<_>>();
        assert_eq!(new_indexes.len(), 1, "expected one new fetch pack index");
        let index_arg = new_indexes[0].to_string_lossy();
        let verify = String::from_utf8(run_success(
            "git",
            &actual_repo,
            &["verify-pack", "-v", &index_arg],
        ))
        .expect("verify-pack output is utf8");
        assert!(
            verify.contains(&head),
            "new fetch pack should contain new commit {head}\n{verify}"
        );
        assert!(
            !verify.contains(&base),
            "new fetch pack should exclude existing have {base}\n{verify}"
        );
        let imported = run_success(
            "git",
            &actual_repo,
            &["cat-file", "-p", &format!("{head}:payload.txt")],
        );
        assert_eq!(imported, b"changed payload\n");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[test]
fn bundle_unbundle_matches_upstream_git_and_imports_objects() {
    let root = unique_temp_dir("bundle-unbundle");
    let source = root.join("source");
    let expected_repo = root.join("expected");
    let actual_repo = root.join("actual");
    fs::create_dir_all(&source).expect("create source repo");
    fs::create_dir_all(&expected_repo).expect("create expected repo");
    fs::create_dir_all(&actual_repo).expect("create actual repo");
    let result = (|| {
        let (bundle, head) = create_bundle_fixture(&source);
        run_success("git", &expected_repo, &["init", "-q"]);
        run_success("git", &actual_repo, &["init", "-q"]);
        let bundle = bundle.to_str().expect("bundle path is utf8");

        for args in [
            vec!["bundle", "unbundle", bundle],
            vec!["bundle", "unbundle", bundle, "refs/heads/feature/topic"],
        ] {
            let expected = run("git", &expected_repo, &args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &actual_repo, &args);
            assert_same_output(actual, expected, &args);
        }

        let imported_type = run_success("git", &actual_repo, &["cat-file", "-t", head.as_str()]);
        assert_eq!(imported_type, b"commit\n");
        let actual_git_dir = actual_repo.join(".git");
        let (_pack_path, index_path) = repository_pack_pair(&actual_git_dir);
        let index_arg = index_path.to_string_lossy();
        run_success("git", &actual_repo, &["verify-pack", "-v", &index_arg]);
        assert!(
            !loose_object_path(&actual_git_dir, &head).exists(),
            "unbundled commit should be stored in pack, not as loose object"
        );
        let refs = run("git", &actual_repo, &["show-ref"]);
        assert_eq!(refs.status.code(), Some(1));
        assert!(refs.stdout.is_empty(), "unbundle should not create refs");
    })();
    let _ = fs::remove_dir_all(&root);
    result
}
