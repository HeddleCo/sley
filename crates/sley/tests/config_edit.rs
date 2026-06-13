use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sley::{ConfigEditError, ConfigEditScope, ConfigSource, Repository};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "sley-config-edit-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn repository_config_with_sources_reports_local_config_path() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let config_path = repo.common_dir().join("config");
    let mut contents = fs::read(&config_path).expect("read config");
    contents.extend_from_slice(b"[User]\n\tName = Local Person\n");
    fs::write(&config_path, contents).expect("write config");

    let snapshot = repo.config_with_sources().expect("snapshot");
    let value = snapshot
        .get("user.name")
        .expect("lookup")
        .filter(|value| value.value.as_deref() == Some("Local Person"))
        .expect("local user.name");

    assert_eq!(
        value.source,
        ConfigSource::Local {
            path: config_path.clone()
        }
    );
}

#[test]
fn repository_apply_config_edit_plan_preserves_comments_and_removes_lock() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let config_path = repo.common_dir().join("config");
    fs::write(
        &config_path,
        b"# keep me\n[user]\n\tname = Old Person\n[core]\n\trepositoryformatversion = 0\n",
    )
    .expect("write config");

    let plan = repo
        .plan_config_set("user.name", "New Person", ConfigEditScope::Local)
        .expect("plan set");
    repo.apply_config_edit_plan(plan).expect("apply set");

    let updated = fs::read_to_string(&config_path).expect("read updated");
    assert!(updated.contains("# keep me\n"));
    assert!(updated.contains("\tname = New Person\n"));
    assert!(updated.contains("[core]\n\trepositoryformatversion = 0\n"));
    assert!(!config_path.with_file_name("config.lock").exists());
}

#[test]
fn repository_apply_config_edit_plan_existing_lock_preserves_original() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let config_path = repo.common_dir().join("config");
    fs::write(&config_path, b"[user]\n\tname = Old Person\n").expect("write config");
    fs::write(config_path.with_file_name("config.lock"), b"held\n").expect("write lock");

    let plan = repo
        .plan_config_set("user.name", "New Person", ConfigEditScope::Local)
        .expect("plan set");
    let err = repo
        .apply_config_edit_plan(plan)
        .expect_err("held lock must fail");

    assert!(matches!(err, ConfigEditError::Locked { .. }));
    assert_eq!(
        fs::read(&config_path).expect("read original"),
        b"[user]\n\tname = Old Person\n"
    );
    assert_eq!(
        fs::read(config_path.with_file_name("config.lock")).expect("read lock"),
        b"held\n"
    );
}

#[test]
fn repository_plan_existing_external_include_is_refused_by_default() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let outside = temp.path.join("outside.cfg");
    let repo = Repository::init(&repo_dir).expect("init");
    fs::write(&outside, b"[user]\n\tname = Included Person\n").expect("write include");
    fs::write(
        repo.common_dir().join("config"),
        format!("[include]\n\tpath = {}\n", outside.display()),
    )
    .expect("write config");

    let snapshot = repo.config_with_sources().expect("snapshot");
    let value = snapshot.get("user.name").expect("lookup").expect("value");
    assert!(matches!(
        &value.source,
        ConfigSource::Included {
            included_from: Some(parent),
            ..
        } if parent == &repo.common_dir().join("config")
    ));

    let err = repo
        .plan_config_edit(
            "user.name",
            ConfigEditScope::ExistingValue {
                allow_external_includes: false,
            },
        )
        .expect_err("external include must be refused");

    assert!(matches!(
        err,
        ConfigEditError::RefusesExternalInclude { path } if path == outside
    ));
}

#[test]
fn repository_plan_existing_include_inside_repo_edits_defining_file() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let repo = Repository::init(&repo_dir).expect("init");
    let included = repo_dir.join("remotes.cfg");
    fs::write(
        &included,
        b"# included\n[remote.Origin] ; keep header comment\n\turl = old-url # touched line\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
    )
    .expect("write include");
    fs::write(
        repo.common_dir().join("config"),
        format!("[include]\n\tpath = {}\n", included.display()),
    )
    .expect("write config");

    let plan = repo
        .plan_config_set(
            "remote.origin.url",
            "new-url",
            ConfigEditScope::ExistingValue {
                allow_external_includes: false,
            },
        )
        .expect("included config inside the repo is editable");

    assert_eq!(plan.target_path, included);
    repo.apply_config_edit_plan(plan).expect("apply set");
    assert_eq!(
        fs::read_to_string(repo.common_dir().join("config")).expect("read main"),
        format!("[include]\n\tpath = {}\n", included.display())
    );
    let updated = fs::read_to_string(&included).expect("read include");
    assert!(updated.contains("[remote.Origin] ; keep header comment\n"));
    assert!(updated.contains("\turl = new-url\n"));
    assert!(updated.contains("\tfetch = +refs/heads/*:refs/remotes/origin/*\n"));
}

#[test]
fn repository_plan_existing_external_include_can_be_opted_in() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let outside = temp.path.join("outside.cfg");
    let repo = Repository::init(&repo_dir).expect("init");
    fs::write(&outside, b"[remote \"origin\"]\n\turl = old-url\n").expect("write include");
    fs::write(
        repo.common_dir().join("config"),
        format!("[include]\n\tpath = {}\n", outside.display()),
    )
    .expect("write config");

    let plan = repo
        .plan_config_set(
            "remote.origin.url",
            "new-url",
            ConfigEditScope::ExistingValue {
                allow_external_includes: true,
            },
        )
        .expect("external include is editable when allowed");

    assert_eq!(plan.target_path, outside);
    repo.apply_config_edit_plan(plan).expect("apply set");
    assert_eq!(
        fs::read_to_string(&outside).expect("read include"),
        "[remote \"origin\"]\n\turl = new-url\n"
    );
}
