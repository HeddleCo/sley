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

fn run(program: &str, cwd: &Path, args: &[&str]) {
    let output = run_output(program, cwd, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_output(program: &str, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn run_output_with_env(program: &str, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(program)
        .current_dir(cwd)
        .args(args)
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"))
}

fn git_rs(cwd: &Path, args: &[&str]) -> Output {
    run_output(env!("CARGO_BIN_EXE_sley"), cwd, args)
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run_output(sley_testkit::oracle_git(), cwd, args)
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

#[test]
fn count_objects_git_object_directory_matches_upstream_git() {
    let root = unique_temp_dir("count-objects-git-object-directory");
    let expected = root.join("expected");
    let actual = root.join("actual");
    fs::create_dir_all(&expected).expect("create expected repo");
    fs::create_dir_all(&actual).expect("create actual repo");
    {
        run(sley_testkit::oracle_git(), &expected, &["init", "-q"]);
        run(sley_testkit::oracle_git(), &actual, &["init", "-q"]);
        for repo in [&expected, &actual] {
            fs::create_dir_all(repo.join("custom-objects")).expect("create custom objects dir");
            fs::write(repo.join("one.txt"), b"one\n").expect("write one");
            let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
            let output = run_output_with_env(sley_testkit::oracle_git(), repo, &["hash-object", "-w", "one.txt"], &envs);
            assert!(
                output.status.success(),
                "hash-object failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let envs = [("GIT_OBJECT_DIRECTORY", "custom-objects")];
        for args in [
            vec!["count-objects"],
            vec!["count-objects", "-v"],
            vec!["count-objects", "-vH"],
        ] {
            let expected_output = run_output_with_env(sley_testkit::oracle_git(), &expected, &args, &envs);
            let actual_output =
                run_output_with_env(env!("CARGO_BIN_EXE_sley"), &actual, &args, &envs);
            assert_same_output(actual_output, expected_output, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn count_objects_matches_upstream_git_for_loose_objects() {
    let root = unique_temp_dir("count-objects");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");
        run(sley_testkit::oracle_git(), &root, &["hash-object", "-w", "one.txt"]);
        run(sley_testkit::oracle_git(), &root, &["hash-object", "-w", "two.txt"]);

        for args in [
            vec!["count-objects"],
            vec!["count-objects", "-H"],
            vec!["count-objects", "-v"],
            vec!["count-objects", "-vH"],
            vec!["count-objects", "-Hv"],
            vec!["count-objects", "--verbose", "--human-readable"],
            vec!["count-objects", "--verbose", "--no-verbose"],
            vec!["count-objects", "--human-readable", "--no-human-readable"],
            vec!["count-objects", "--no-verbose", "-v"],
            vec!["count-objects", "--no-human-readable", "-H"],
            vec!["count-objects", "-vH", "--no-human-readable"],
            vec!["count-objects", "-vH", "--no-verbose"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn count_objects_matches_upstream_git_for_packed_objects() {
    let root = unique_temp_dir("count-objects-packed");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        fs::write(root.join("two.txt"), b"two\n").expect("write two");
        run(sley_testkit::oracle_git(), &root, &["add", "."]);
        run(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=A U Thor",
                "-c",
                "user.email=author@example.com",
                "commit",
                "-qm",
                "init",
            ],
        );
        run(sley_testkit::oracle_git(), &root, &["gc", "--quiet"]);

        for args in [
            vec!["count-objects"],
            vec!["count-objects", "-v"],
            vec!["count-objects", "-vH"],
            vec!["count-objects", "--verbose", "--human-readable"],
            vec!["count-objects", "--verbose", "--no-verbose"],
            vec!["count-objects", "-vH", "--no-human-readable"],
            vec!["count-objects", "-vH", "--no-verbose"],
        ] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn count_objects_prune_packable_and_garbage_match_upstream_git() {
    let root = unique_temp_dir("count-objects-prune-garbage");
    fs::create_dir_all(&root).expect("create temp repo");
    {
        run(sley_testkit::oracle_git(), &root, &["init", "-q"]);
        fs::write(root.join("one.txt"), b"one\n").expect("write one");
        let hash_output = git(&root, &["hash-object", "-w", "one.txt"]);
        assert!(hash_output.status.success(), "hash-object failed");
        let oid = String::from_utf8(hash_output.stdout)
            .expect("oid is utf8")
            .trim()
            .to_string();
        let loose_path = root.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        let loose_body = fs::read(&loose_path).expect("read loose object");
        run(sley_testkit::oracle_git(), &root, &["add", "one.txt"]);
        run(
            sley_testkit::oracle_git(),
            &root,
            &[
                "-c",
                "user.name=A U Thor",
                "-c",
                "user.email=author@example.com",
                "commit",
                "-qm",
                "init",
            ],
        );
        run(sley_testkit::oracle_git(), &root, &["gc", "--quiet"]);
        fs::create_dir_all(loose_path.parent().expect("loose parent")).expect("recreate fanout");
        fs::write(&loose_path, loose_body).expect("restore packed loose object");
        let garbage_dir = root.join(".git/objects/aa");
        fs::create_dir_all(&garbage_dir).expect("create garbage fanout");
        fs::write(garbage_dir.join("not-an-object"), b"garbage").expect("write garbage");
        fs::write(root.join(".git/objects/top-level-garbage"), b"ignored")
            .expect("write top-level garbage");

        for args in [vec!["count-objects", "-v"], vec!["count-objects", "-vH"]] {
            let expected = git(&root, &args);
            let actual = git_rs(&root, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn count_objects_verbose_lists_alternates_like_upstream_git() {
    let root = unique_temp_dir("count-objects-alternates");
    let repo = root.join("repo");
    let alternate = root.join("alternate");
    fs::create_dir_all(&repo).expect("create repo");
    fs::create_dir_all(&alternate).expect("create alternate repo");
    {
        run(sley_testkit::oracle_git(), &alternate, &["init", "-q"]);
        fs::write(alternate.join("alt.txt"), b"alt\n").expect("write alternate object");
        run(sley_testkit::oracle_git(), &alternate, &["hash-object", "-w", "alt.txt"]);
        run(sley_testkit::oracle_git(), &repo, &["init", "-q"]);
        fs::create_dir_all(repo.join(".git/objects/info")).expect("create info dir");
        fs::write(
            repo.join(".git/objects/info/alternates"),
            format!(
                "# ignored\n\n{}\n",
                alternate.join(".git/objects").display()
            ),
        )
        .expect("write alternates");

        for args in [vec!["count-objects", "-v"], vec!["count-objects", "-vH"]] {
            let expected = git(&repo, &args);
            let actual = git_rs(&repo, &args);
            assert_same_output(actual, expected, &args);
        }
    };
    let _ = fs::remove_dir_all(&root);
}
