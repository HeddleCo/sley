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

fn run_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write child stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_with_env_and_stdin(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> Output {
    let mut child = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write child stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"))
}

fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = run(program, cwd, args);
    assert_success(program, args, &output);
    output.stdout
}

fn run_success_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let output = run_with_stdin(program, cwd, args, stdin);
    assert_success(program, args, &output);
    output.stdout
}

fn run_success_with_env_and_stdin(
    program: &str,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> Vec<u8> {
    let output = run_with_env_and_stdin(program, cwd, args, envs, stdin);
    assert_success(program, args, &output);
    output.stdout
}

fn assert_success(program: &str, args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

fn create_pack(root: &Path, body: &[u8]) -> String {
    create_named_pack(root, body).0
}

fn create_named_pack(root: &Path, body: &[u8]) -> (String, String) {
    let oid = run_success_with_stdin(sley_testkit::oracle_git(), root, &["hash-object", "-w", "--stdin"], body);
    let oid = String::from_utf8(oid)
        .expect("object id is utf8")
        .trim()
        .to_string();
    let pack_prefix = root.join(".git").join("objects").join("pack").join("pack");
    let input = format!("{oid}\n");
    let pack_hash = run_success_with_stdin(
        sley_testkit::oracle_git(),
        root,
        &[
            "pack-objects",
            pack_prefix.to_str().expect("pack prefix is utf8"),
        ],
        input.as_bytes(),
    );
    let pack_hash = String::from_utf8(pack_hash)
        .expect("pack hash is utf8")
        .trim()
        .to_string();
    (oid, format!("pack-{pack_hash}.idx"))
}

fn create_named_pack_in_object_dir(root: &Path, object_dir: &str, body: &[u8]) -> (String, String) {
    let envs = [("GIT_OBJECT_DIRECTORY", object_dir)];
    fs::create_dir_all(root.join(object_dir)).expect("create custom object dir");
    let oid =
        run_success_with_env_and_stdin(sley_testkit::oracle_git(), root, &["hash-object", "-w", "--stdin"], &envs, body);
    let oid = String::from_utf8(oid)
        .expect("object id is utf8")
        .trim()
        .to_string();
    let pack_dir = root.join(object_dir).join("pack");
    fs::create_dir_all(&pack_dir).expect("create custom pack dir");
    let pack_prefix = pack_dir.join("pack");
    let input = format!("{oid}\n");
    let pack_hash = run_success_with_env_and_stdin(
        sley_testkit::oracle_git(),
        root,
        &[
            "pack-objects",
            pack_prefix.to_str().expect("pack prefix is utf8"),
        ],
        &envs,
        input.as_bytes(),
    );
    let pack_hash = String::from_utf8(pack_hash)
        .expect("pack hash is utf8")
        .trim()
        .to_string();
    (oid, format!("pack-{pack_hash}.idx"))
}

#[test]
fn multi_pack_index_write_matches_upstream_and_verifies() {
    let root = unique_temp_dir("midx-write");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        let first = create_pack(&root, b"first midx object\n");
        let second = create_pack(&root, b"second midx object\n");
        let args = ["multi-pack-index", "write"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let midx_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join("multi-pack-index");
        assert!(
            midx_path.exists(),
            "upstream did not write multi-pack-index"
        );
        fs::remove_file(&midx_path).expect("remove upstream multi-pack-index");

        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        assert!(midx_path.exists(), "sley did not write multi-pack-index");
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "verify"]);
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &root, &["cat-file", "-p", &first]),
            b"first midx object\n"
        );
        assert_eq!(
            run_success(sley_testkit::oracle_git(), &root, &["cat-file", "-p", &second]),
            b"second midx object\n"
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_write_object_dir_matches_upstream() {
    let root = unique_temp_dir("midx-write-object-dir");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        create_pack(&root, b"object-dir midx object\n");
        let args = ["multi-pack-index", "write", "--object-dir=.git/objects"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let midx_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join("multi-pack-index");
        assert!(
            midx_path.exists(),
            "upstream did not write multi-pack-index"
        );
        fs::remove_file(&midx_path).expect("remove upstream multi-pack-index");

        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "verify"]);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_git_object_directory_default_matches_upstream_git() {
    let root = unique_temp_dir("midx-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo dir");
    fs::create_dir_all(&actual).expect("create actual repo dir");
    {
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        for repo in [&expected, &actual] {
            run_success(sley_testkit::oracle_git(), repo, &["init", "-q"]);
            create_named_pack_in_object_dir(repo, "custom-objects", b"custom midx object\n");
        }

        let args = ["multi-pack-index", "write"];
        let expected_output = run_with_env(sley_testkit::oracle_git(), &expected, &args, &envs);
        let actual_output = run_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &envs);
        assert_same_output(actual_output, expected_output, &args);

        for repo in [&expected, &actual] {
            assert!(
                repo.join("custom-objects")
                    .join("pack")
                    .join("multi-pack-index")
                    .exists(),
                "multi-pack-index was not written to GIT_OBJECT_DIRECTORY"
            );
            assert!(
                !repo
                    .join(".git")
                    .join("objects")
                    .join("pack")
                    .join("multi-pack-index")
                    .exists(),
                "multi-pack-index was written to the default object directory"
            );
        }

        let verify_args = ["multi-pack-index", "verify"];
        let expected_verify = run_with_env(sley_testkit::oracle_git(), &expected, &verify_args, &envs);
        let actual_verify = run_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &verify_args, &envs);
        assert_same_output(actual_verify, expected_verify, &verify_args);
        let actual_upstream_verify = run_with_env(sley_testkit::oracle_git(), &actual, &verify_args, &envs);
        assert_success(sley_testkit::oracle_git(), &verify_args, &actual_upstream_verify);

        let expire_args = ["multi-pack-index", "expire"];
        let expected_expire = run_with_env(sley_testkit::oracle_git(), &expected, &expire_args, &envs);
        let actual_expire = run_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &expire_args, &envs);
        assert_same_output(actual_expire, expected_expire, &expire_args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_write_stdin_packs_matches_upstream() {
    let root = unique_temp_dir("midx-write-stdin-packs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        let (_first, first_pack) = create_named_pack(&root, b"stdin first midx object\n");
        let (_second, second_pack) = create_named_pack(&root, b"stdin second midx object\n");
        let args = ["multi-pack-index", "write", "--stdin-packs"];
        let stdin = format!("{second_pack}\n");
        let expected = run_with_stdin(sley_testkit::oracle_git(), &root, &args, stdin.as_bytes());
        let midx_path = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join("multi-pack-index");
        assert!(
            midx_path.exists(),
            "upstream did not write multi-pack-index"
        );
        fs::remove_file(&midx_path).expect("remove upstream multi-pack-index");

        let actual = run_with_stdin(env!("CARGO_BIN_EXE_sley"), &root, &args, stdin.as_bytes());
        assert_same_output(actual, expected, &args);
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "verify"]);
        assert!(midx_path.exists(), "sley did not write multi-pack-index");
        assert!(
            root.join(".git")
                .join("objects")
                .join("pack")
                .join(first_pack)
                .exists()
        );
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_verify_matches_upstream_git() {
    let root = unique_temp_dir("midx-verify");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        create_pack(&root, b"verify first midx object\n");
        create_pack(&root, b"verify second midx object\n");
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "write"]);

        for args in [
            ["multi-pack-index", "verify"].as_slice(),
            ["multi-pack-index", "verify", "--object-dir=.git/objects"].as_slice(),
            ["multi-pack-index", "verify", "--no-progress"].as_slice(),
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, args);
            assert_same_output(actual, expected, args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_expire_quiet_baseline_matches_upstream_git() {
    let root = unique_temp_dir("midx-expire");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        let args = ["multi-pack-index", "expire"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(env!("CARGO_BIN_EXE_sley"), &root, &args);
        assert_same_output(actual, expected, &args);

        create_pack(&root, b"expire first midx object\n");
        create_pack(&root, b"expire second midx object\n");
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "write"]);
        for args in [
            ["multi-pack-index", "expire"].as_slice(),
            ["multi-pack-index", "expire", "--object-dir=.git/objects"].as_slice(),
            ["multi-pack-index", "expire", "--no-progress"].as_slice(),
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, args);
            let actual = run(env!("CARGO_BIN_EXE_sley"), &root, args);
            assert_same_output(actual, expected, args);
        }
        run_success(sley_testkit::oracle_git(), &root, &["multi-pack-index", "verify"]);
    };
    let _ = fs::remove_dir_all(&root);
}
