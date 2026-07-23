use std::fs;
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
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("child stdin"),
        stdin,
    );
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
    sley_testkit::write_stdin_tolerating_early_exit(
        child.stdin.as_mut().expect("child stdin"),
        stdin,
    );
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
    let oid = run_success_with_stdin(
        sley_testkit::oracle_git(),
        root,
        &["hash-object", "-w", "--stdin"],
        body,
    );
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
    let oid = run_success_with_env_and_stdin(
        sley_testkit::oracle_git(),
        root,
        &["hash-object", "-w", "--stdin"],
        &envs,
        body,
    );
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
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
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

        let actual = run(sley_testkit::sley_bin!(), &root, &args);
        assert_same_output(actual, expected, &args);
        assert!(midx_path.exists(), "sley did not write multi-pack-index");
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "verify"],
        );
        assert_eq!(
            run_success(
                sley_testkit::oracle_git(),
                &root,
                &["cat-file", "-p", &first]
            ),
            b"first midx object\n"
        );
        assert_eq!(
            run_success(
                sley_testkit::oracle_git(),
                &root,
                &["cat-file", "-p", &second]
            ),
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
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
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

        let actual = run(sley_testkit::sley_bin!(), &root, &args);
        assert_same_output(actual, expected, &args);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "verify"],
        );
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
            run_success(
                sley_testkit::oracle_git(),
                repo,
                &["init", "-q", "-b", "main"],
            );
            create_named_pack_in_object_dir(repo, "custom-objects", b"custom midx object\n");
        }

        let args = ["multi-pack-index", "write"];
        let expected_output = run_with_env(sley_testkit::oracle_git(), &expected, &args, &envs);
        let actual_output = run_with_env(sley_testkit::sley_bin!(), &actual, &args, &envs);
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
        let expected_verify =
            run_with_env(sley_testkit::oracle_git(), &expected, &verify_args, &envs);
        let actual_verify = run_with_env(sley_testkit::sley_bin!(), &actual, &verify_args, &envs);
        assert_same_output(actual_verify, expected_verify, &verify_args);
        let actual_upstream_verify =
            run_with_env(sley_testkit::oracle_git(), &actual, &verify_args, &envs);
        assert_success(
            sley_testkit::oracle_git(),
            &verify_args,
            &actual_upstream_verify,
        );

        let expire_args = ["multi-pack-index", "expire"];
        let expected_expire =
            run_with_env(sley_testkit::oracle_git(), &expected, &expire_args, &envs);
        let actual_expire = run_with_env(sley_testkit::sley_bin!(), &actual, &expire_args, &envs);
        assert_same_output(actual_expire, expected_expire, &expire_args);
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_write_stdin_packs_matches_upstream() {
    let root = unique_temp_dir("midx-write-stdin-packs");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
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

        let actual = run_with_stdin(sley_testkit::sley_bin!(), &root, &args, stdin.as_bytes());
        assert_same_output(actual, expected, &args);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "verify"],
        );
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
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        create_pack(&root, b"verify first midx object\n");
        create_pack(&root, b"verify second midx object\n");
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "write"],
        );

        for args in [
            ["multi-pack-index", "verify"].as_slice(),
            ["multi-pack-index", "verify", "--object-dir=.git/objects"].as_slice(),
            ["multi-pack-index", "verify", "--no-progress"].as_slice(),
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, args);
            let actual = run(sley_testkit::sley_bin!(), &root, args);
            assert_same_output(actual, expected, args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cat_file_objectsize_disk_uses_midx_when_pack_side_is_missing() {
    let root = unique_temp_dir("midx-objectsize-disk");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["config", "core.multiPackIndex", "true"],
        );
        fs::write(root.join("base.t"), "base\n").expect("write fixture");
        run_success(sley_testkit::oracle_git(), &root, &["add", "base.t"]);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=A U Thor",
                "-c",
                "user.email=author@example.com",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        run_success(sley_testkit::oracle_git(), &root, &["repack", "-ad"]);
        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["multi-pack-index", "write"],
        );

        let tip = String::from_utf8(run_success(
            sley_testkit::oracle_git(),
            &root,
            &["rev-parse", "HEAD"],
        ))
        .expect("tip is utf8");
        let pack_dir = root.join(".git").join("objects").join("pack");
        let idx = fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("idx"))
            .expect("pack index exists");
        let pack = idx.with_extension("pack");
        let idx_backup = idx.with_extension("idx.bak");
        let pack_backup = pack.with_extension("pack.bak");
        let args = ["cat-file", "--batch-check=%(objectsize:disk)"];

        fs::rename(&idx, &idx_backup).expect("hide pack index");
        let output = run_with_stdin(sley_testkit::sley_bin!(), &root, &args, tip.as_bytes());
        assert_success(sley_testkit::sley_bin!(), &args, &output);
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u64>()
                .is_ok(),
            "objectsize:disk should be numeric, got {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        fs::rename(&idx_backup, &idx).expect("restore pack index");

        fs::rename(&pack, &pack_backup).expect("hide pack data");
        let output = run_with_stdin(sley_testkit::sley_bin!(), &root, &args, tip.as_bytes());
        assert_success(sley_testkit::sley_bin!(), &args, &output);

        let rewrite = run(
            sley_testkit::sley_bin!(),
            &root,
            &["multi-pack-index", "write"],
        );
        assert!(
            !rewrite.status.success(),
            "midx rewrite should fail when a named pack is missing"
        );
        assert!(
            String::from_utf8_lossy(&rewrite.stderr).contains("could not load pack"),
            "stderr did not mention missing pack:\n{}",
            String::from_utf8_lossy(&rewrite.stderr)
        );
        fs::rename(&pack_backup, &pack).expect("restore pack data");
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multi_pack_index_expire_quiet_baseline_matches_upstream_git() {
    let root = unique_temp_dir("midx-expire");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        let args = ["multi-pack-index", "expire"];
        let expected = run(sley_testkit::oracle_git(), &root, &args);
        let actual = run(sley_testkit::sley_bin!(), &root, &args);
        assert_same_output(actual, expected, &args);

        create_pack(&root, b"expire first midx object\n");
        create_pack(&root, b"expire second midx object\n");
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "write"],
        );
        for args in [
            ["multi-pack-index", "expire"].as_slice(),
            ["multi-pack-index", "expire", "--object-dir=.git/objects"].as_slice(),
            ["multi-pack-index", "expire", "--no-progress"].as_slice(),
        ] {
            let expected = run(sley_testkit::oracle_git(), &root, args);
            let actual = run(sley_testkit::sley_bin!(), &root, args);
            assert_same_output(actual, expected, args);
        }
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["multi-pack-index", "verify"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}

/// t5319 "expire removes repacked packs": after a batch-size midx repack that
/// folds the smallest packs into a new one, expire must drop only the packs
/// whose objects are fully covered by the new pack (leaving the four largest).
/// Sensitive to pack-objects size parity (batch selection) and to midx
/// preferred-copy attribution after the repack rewrite.
#[test]
fn multi_pack_index_expire_removes_repacked_packs() {
    let root = unique_temp_dir("midx-expire-repacked");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["init", "-q", "-b", "main"],
        );
        // Incompressible payload so pack-E (base history) stays among the
        // largest packs, matching t5319's genrandom fixture shape.
        let mut large = vec![0u8; 4096];
        for (i, byte) in large.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(37).wrapping_add(91) % 251) as u8;
        }
        fs::write(root.join("large.txt"), &large).expect("write large");
        run_success(sley_testkit::oracle_git(), &root, &["add", "large.txt"]);
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=A U Thor",
                "-c",
                "user.email=author@example.com",
                "commit",
                "-q",
                "-m",
                "base",
            ],
        );
        for i in 1..=20 {
            let name = format!("f{i}.txt");
            fs::write(root.join(&name), format!("content {i}\n")).expect("write file");
            run_success(sley_testkit::oracle_git(), &root, &["add", &name]);
            run_success(
                sley_testkit::oracle_git(),
                &root,
                &[
                    "-c",
                    "user.name=A U Thor",
                    "-c",
                    "user.email=author@example.com",
                    "commit",
                    "-q",
                    "-m",
                    &format!("c{i}"),
                ],
            );
        }
        // Build five disjoint packs A..E (same topology as t5319 setup expire).
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "A", "HEAD"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "B", "HEAD~8"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "C", "HEAD~13"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "D", "HEAD~16"],
        );
        run_success(
            sley_testkit::oracle_git(),
            &root,
            &["branch", "E", "HEAD~18"],
        );
        let pack_prefix = root
            .join(".git")
            .join("objects")
            .join("pack")
            .join("pack");
        let pack_prefix = pack_prefix.to_str().expect("utf8 pack prefix");
        for (name, stdin) in [
            ("A", "refs/heads/A\n^refs/heads/B\n"),
            ("B", "refs/heads/B\n^refs/heads/C\n"),
            ("C", "refs/heads/C\n^refs/heads/D\n"),
            ("D", "refs/heads/D\n^refs/heads/E\n"),
            ("E", "refs/heads/E\n"),
        ] {
            let prefix = format!("{pack_prefix}-{name}");
            // Use sley's pack-objects so sizes match what the upstream suite sees.
            run_success_with_stdin(
                sley_testkit::sley_bin!(),
                &root,
                &["pack-objects", "--revs", &prefix],
                stdin.as_bytes(),
            );
        }
        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["multi-pack-index", "write"],
        );

        let pack_dir = root.join(".git").join("objects").join("pack");
        let mut packs: Vec<PathBuf> = fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pack"))
            .collect();
        assert_eq!(packs.len(), 5, "setup should leave five packs");
        packs.sort_by_key(|path| {
            // Prefer named pack-D..A order for aging; fall back to size.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let rank = if name.contains("pack-D-") {
                0
            } else if name.contains("pack-C-") {
                1
            } else if name.contains("pack-B-") {
                2
            } else if name.contains("pack-A-") {
                3
            } else {
                4
            };
            (rank, fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        });
        // Match t5319: age D,C,B,A so batch selection visits smallest/oldest first.
        for (i, pack) in packs.iter().take(4).enumerate() {
            let stamp = format!("20200101000{}", i + 1);
            for path in [
                pack.clone(),
                pack.with_extension("idx"),
                pack.with_extension("rev"),
            ] {
                if path.exists() {
                    let status = Command::new("touch")
                        .args(["-t", &stamp])
                        .arg(&path)
                        .status()
                        .expect("touch pack");
                    assert!(status.success(), "touch failed for {}", path.display());
                }
            }
        }

        let mut pack_sizes: Vec<u64> = packs
            .iter()
            .filter_map(|path| fs::metadata(path).ok().map(|m| m.len()))
            .collect();
        pack_sizes.sort_unstable();
        let batch = pack_sizes[2] + 1;

        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &[
                "multi-pack-index",
                "repack",
                &format!("--batch-size={batch}"),
            ],
        );

        let after_repack: Vec<PathBuf> = fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pack"))
            .collect();
        assert_eq!(
            after_repack.len(),
            6,
            "batch repack should add exactly one pack"
        );

        // Four largest packs before expire are the expected survivors.
        let mut sized: Vec<(u64, PathBuf)> = after_repack
            .iter()
            .filter_map(|path| {
                fs::metadata(path)
                    .ok()
                    .map(|m| (m.len(), path.clone()))
            })
            .collect();
        sized.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let expect: Vec<PathBuf> = sized.iter().take(4).map(|(_, p)| p.clone()).collect();

        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["multi-pack-index", "expire"],
        );

        let mut actual: Vec<PathBuf> = fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("pack"))
            .collect();
        actual.sort();
        let mut expect_sorted = expect;
        expect_sorted.sort();
        assert_eq!(
            actual, expect_sorted,
            "expire should leave the four largest packs"
        );

        // MIDX must list the remaining packs only.
        let midx = fs::read(pack_dir.join("multi-pack-index")).expect("read midx");
        let idx_count = actual.len();
        // Header: signature(4) version(1) oid_ver(1) chunks(1) base(1) num_packs(4)
        assert!(midx.len() >= 12, "midx too short");
        let num_packs = u32::from_be_bytes(midx[8..12].try_into().expect("4 bytes"));
        assert_eq!(
            num_packs as usize, idx_count,
            "midx pack count should match remaining packs"
        );

        run_success(
            sley_testkit::sley_bin!(),
            &root,
            &["multi-pack-index", "verify"],
        );
    };
    let _ = fs::remove_dir_all(&root);
}
