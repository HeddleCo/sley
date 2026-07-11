use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "sley-scalar-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create fixture root");
    path
}

fn run(program: &Path, cwd: &Path, args: &[&str]) -> Output {
    run_configured(program, cwd, args, &[])
}

fn run_configured(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    environment: &[(&str, &Path)],
) -> Output {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Scalar Test")
        .env("GIT_AUTHOR_EMAIL", "scalar@example.com")
        .env("GIT_COMMITTER_NAME", "Scalar Test")
        .env("GIT_COMMITTER_EMAIL", "scalar@example.com")
        .env(
            "GIT_TEST_MAINT_SCHEDULER",
            "crontab:true,systemctl:true,launchctl:true,schtasks:true",
        );
    for (name, value) in environment {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|err| panic!("run {} {args:?}: {err}", program.display()))
}

fn scalar_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_scalar"))
}

fn sley_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_sley"))
}

fn scalar_clone_fixture(root: &Path) -> PathBuf {
    let origin = root.join("origin");
    let init = run(
        sley_bin(),
        root,
        &["init", origin.to_str().expect("utf8 origin path")],
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(origin.join("top"), b"top\n").expect("write top-level file");
    fs::create_dir_all(origin.join("1/2")).expect("create nested directory");
    fs::write(origin.join("1/2/3"), b"nested\n").expect("write nested file");
    for args in [
        &["add", "."][..],
        &["commit", "-m", "initial"][..],
        &["config", "uploadPack.allowFilter", "true"][..],
        &["config", "uploadPack.allowAnySHA1InWant", "true"][..],
    ] {
        let output = run(sley_bin(), &origin, args);
        assert!(
            output.status.success(),
            "sley {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    origin
}

fn scalar_clone_environment(root: &Path) -> Vec<(&'static str, PathBuf)> {
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create isolated home");
    vec![
        ("HOME", home),
        ("GIT_CONFIG_GLOBAL", root.join("global-config")),
    ]
}

#[test]
fn scalar_diagnose_missing_directory_matches_oracle_bytes() {
    let oracle_git = Path::new(sley_testkit::oracle_git());
    let Some(oracle_dir) = oracle_git.parent() else {
        return;
    };
    let oracle_scalar = oracle_dir.join("scalar");
    if !oracle_scalar.is_file() {
        return;
    }
    let root = unique_temp_dir("missing");
    let missing = "definitely-missing";
    let expected = run(&oracle_scalar, &root, &["diagnose", missing]);
    let actual = run(scalar_bin(), &root, &["diagnose", missing]);

    assert_eq!(actual.status.code(), expected.status.code());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_diagnose_writes_archive_at_enlistment_root() {
    let root = unique_temp_dir("diagnose");
    let enlistment = root.join("enlistment");
    let repository = enlistment.join("src");
    let init = run(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let diagnose = run(scalar_bin(), &root, &["diagnose", "enlistment"]);
    assert!(
        diagnose.status.success(),
        "{}",
        String::from_utf8_lossy(&diagnose.stderr)
    );
    assert!(String::from_utf8_lossy(&diagnose.stdout).contains("Available space"));

    let diagnostics = enlistment.join(".scalarDiagnostics");
    let archive = fs::read_dir(&diagnostics)
        .expect("read diagnostics directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "zip"))
        .expect("diagnostics zip");
    let bytes = fs::read(archive).expect("read diagnostics zip");
    for entry in ["diagnostics.log", "packs-local.txt", "objects-local.txt"] {
        assert!(
            bytes
                .windows(entry.len())
                .any(|window| window == entry.as_bytes()),
            "archive is missing {entry}"
        );
    }
    assert!(
        !repository.join(".scalarDiagnostics").exists(),
        "archive was written inside src instead of the enlistment root"
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_clone_creates_sparse_partial_src_enlistment_and_registers_it() {
    let root = unique_temp_dir("clone-src");
    let origin = scalar_clone_fixture(&root);
    let environment = scalar_clone_environment(&root);
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let url = format!("file://{}", origin.display());
    let output = run_configured(
        scalar_bin(),
        &root,
        &["clone", &url, "cloned", "--single-branch"],
        &environment_refs,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let worktree = root.join("cloned/src");
    assert!(worktree.join(".git").is_dir());
    assert!(worktree.join("top").is_file());
    assert!(
        !worktree.join("1/2").exists(),
        "sparse clone populated nested paths"
    );
    let config = fs::read_to_string(worktree.join(".git/config")).expect("read clone config");
    for expected in [
        "promisor = true",
        "partialclonefilter = blob:none",
        "skipHash = true",
        "aheadBehind = false",
    ] {
        assert!(config.contains(expected), "missing {expected}:\n{config}");
    }
    let worktree_config =
        fs::read_to_string(worktree.join(".git/config.worktree")).expect("read worktree config");
    assert!(
        worktree_config.contains("sparsecheckout = true"),
        "{worktree_config}"
    );
    let global = fs::read_to_string(root.join("global-config")).expect("read global config");
    let canonical = fs::canonicalize(&worktree).expect("canonical clone path");
    assert!(global.contains("[scalar]"), "{global}");
    assert!(global.contains("[maintenance]"), "{global}");
    assert!(
        global.contains(&format!("repo = {}", canonical.display())),
        "{global}"
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_clone_no_src_no_tags_emits_scalar_fetch_trace_contract() {
    let root = unique_temp_dir("clone-no-src");
    let origin = scalar_clone_fixture(&root);
    let trace = root.join("trace.json");
    let mut environment = scalar_clone_environment(&root);
    environment.push(("GIT_TRACE2_EVENT", trace.clone()));
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let url = format!("file://{}", origin.display());
    let output = run_configured(
        scalar_bin(),
        &root,
        &[
            "clone",
            "--no-tags",
            "--no-src",
            &url,
            "cloned",
            "--single-branch",
        ],
        &environment_refs,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("cloned/.git").is_dir());
    let config = fs::read_to_string(root.join("cloned/.git/config")).expect("read clone config");
    assert!(config.contains("tagOpt = --no-tags"), "{config}");
    let trace = fs::read_to_string(trace).expect("read trace");
    assert!(
        trace.contains(r#""argv":["git","fetch","--quiet","--no-progress","origin","--no-tags"]"#),
        "{trace}"
    );
    fs::remove_dir_all(root).ok();
}
