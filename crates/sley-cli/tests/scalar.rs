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

#[test]
fn scalar_registration_lifecycle_uses_native_config_and_registry() {
    let root = unique_temp_dir("registration");
    let repository = root.join("enlistment/src");
    let environment = scalar_clone_environment(&root);
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let init = run_configured(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let register = run_configured(
        scalar_bin(),
        &root,
        &["register", "--no-maintenance", "enlistment"],
        &environment_refs,
    );
    assert!(
        register.status.success(),
        "{}",
        String::from_utf8_lossy(&register.stderr)
    );
    let canonical = fs::canonicalize(&repository).expect("canonical repository path");
    let list = run_configured(scalar_bin(), &root, &["list"], &environment_refs);
    assert_eq!(
        String::from_utf8_lossy(&list.stdout),
        format!("{}\n", canonical.display())
    );
    let config = fs::read_to_string(repository.join(".git/config")).expect("read local config");
    assert!(
        config.contains("GCWarning = false # set by scalar"),
        "{config}"
    );
    assert!(
        config.contains("excludeDecoration = refs/prefetch/* # set by scalar"),
        "{config}"
    );
    assert!(
        config.contains("fsmonitor = true # set by scalar"),
        "{config}"
    );
    let watching = run_configured(
        sley_bin(),
        &repository,
        &["fsmonitor--daemon", "status"],
        &environment_refs,
    );
    let git_watching = run_configured(
        Path::new(sley_testkit::oracle_git()),
        &repository,
        &["fsmonitor--daemon", "status"],
        &environment_refs,
    );
    assert_eq!(watching.status.code(), git_watching.status.code());
    assert_eq!(watching.stdout, git_watching.stdout);
    assert_eq!(watching.stderr, git_watching.stderr);

    let unregister = run_configured(
        scalar_bin(),
        &root,
        &["unregister", "enlistment"],
        &environment_refs,
    );
    assert!(
        unregister.status.success(),
        "{}",
        String::from_utf8_lossy(&unregister.stderr)
    );
    let list = run_configured(scalar_bin(), &root, &["list"], &environment_refs);
    assert!(list.stdout.is_empty(), "{:?}", list.stdout);
    let stopped = run_configured(
        sley_bin(),
        &repository,
        &["fsmonitor--daemon", "status"],
        &environment_refs,
    );
    let git_stopped = run_configured(
        Path::new(sley_testkit::oracle_git()),
        &repository,
        &["fsmonitor--daemon", "status"],
        &environment_refs,
    );
    assert_eq!(stopped.status.code(), git_stopped.status.code());
    assert_eq!(stopped.stdout, git_stopped.stdout);
    assert_eq!(stopped.stderr, git_stopped.stderr);
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_register_honors_discovery_ceiling_and_rejects_git_dir() {
    let root = unique_temp_dir("registration-ceiling");
    let repository = root.join("enlistment/src");
    let deep = repository.join("deep");
    fs::create_dir_all(&deep).expect("create deep path");
    let mut environment = scalar_clone_environment(&root);
    environment.push(("GIT_CEILING_DIRECTORIES", repository.clone()));
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let init = run_configured(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(init.status.success());

    let blocked = run_configured(
        scalar_bin(),
        &root,
        &[
            "register",
            "--no-maintenance",
            deep.to_str().expect("utf8 deep path"),
        ],
        &environment_refs,
    );
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("not a git repository"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );

    let git_dir = repository.join(".git");
    let admin = run_configured(
        scalar_bin(),
        &root,
        &[
            "register",
            "--no-maintenance",
            git_dir.to_str().expect("utf8 git dir path"),
        ],
        &environment_refs,
    );
    assert!(!admin.status.success());
    assert!(
        String::from_utf8_lossy(&admin.stderr).contains("Scalar enlistments require a worktree"),
        "{}",
        String::from_utf8_lossy(&admin.stderr)
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_delete_resolves_nested_path_to_registered_worktree() {
    let root = unique_temp_dir("delete-nested");
    let repository = root.join("enlistment");
    let nested = repository.join("nested/directory");
    let environment = scalar_clone_environment(&root);
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let init = run_configured(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::create_dir_all(&nested).expect("create nested input path");
    fs::write(nested.join("keep"), b"nested\n").expect("write nested marker");

    let canonical = fs::canonicalize(&repository).expect("canonical repository path");
    fs::write(
        root.join("global-config"),
        format!(
            "[scalar]\n\trepo = {}\n[maintenance]\n\trepo = {}\n",
            canonical.display(),
            canonical.display()
        ),
    )
    .expect("seed registered enlistment values");
    let delete = run_configured(
        scalar_bin(),
        &root,
        &[
            "delete",
            nested.to_str().expect("utf8 nested repository path"),
        ],
        &environment_refs,
    );
    assert!(
        delete.status.success(),
        "{}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert!(
        !repository.exists(),
        "delete must remove the resolved worktree, not only its nested argument"
    );
    let list = run_configured(scalar_bin(), &root, &["list"], &environment_refs);
    assert!(list.stdout.is_empty(), "{:?}", list.stdout);
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_delete_refuses_target_containing_current_directory() {
    let root = unique_temp_dir("delete-current");
    let repository = root.join("enlistment");
    let nested = repository.join("nested");
    let environment = scalar_clone_environment(&root);
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let init = run_configured(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::create_dir_all(&nested).expect("create nested cwd");
    fs::write(repository.join("marker"), b"preserve\n").expect("write safety marker");

    let delete = run_configured(
        scalar_bin(),
        &nested,
        &["delete", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(!delete.status.success());
    assert!(
        String::from_utf8_lossy(&delete.stderr)
            .contains("refusing to delete current working directory"),
        "{}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert!(repository.join("marker").is_file());
    fs::remove_dir_all(root).ok();
}

#[test]
fn scalar_global_config_edit_respects_existing_lock() {
    let root = unique_temp_dir("global-config-lock");
    let repository = root.join("enlistment");
    let environment = scalar_clone_environment(&root);
    let environment_refs = environment
        .iter()
        .map(|(name, value)| (*name, value.as_path()))
        .collect::<Vec<_>>();
    let init = run_configured(
        sley_bin(),
        &root,
        &["init", repository.to_str().expect("utf8 repository path")],
        &environment_refs,
    );
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let canonical = fs::canonicalize(&repository).expect("canonical repository path");
    let config_path = root.join("global-config");
    let original = format!("[scalar]\n\trepo = {}\n", canonical.display());
    fs::write(&config_path, &original).expect("seed global config");
    let lock_path = root.join("global-config.lock");
    fs::write(&lock_path, b"held by another writer\n").expect("hold global config lock");

    let unregister = run_configured(
        scalar_bin(),
        &root,
        &["unregister", "enlistment"],
        &environment_refs,
    );
    assert!(!unregister.status.success());
    assert!(
        String::from_utf8_lossy(&unregister.stderr).contains("config lock already exists"),
        "{}",
        String::from_utf8_lossy(&unregister.stderr)
    );
    assert_eq!(
        fs::read_to_string(&config_path).expect("read original config"),
        original
    );
    assert_eq!(
        fs::read(&lock_path).expect("read held lock"),
        b"held by another writer\n"
    );
    fs::remove_dir_all(root).ok();
}
