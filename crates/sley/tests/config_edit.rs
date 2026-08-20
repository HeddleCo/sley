use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sley::{
    ConfigEditError, ConfigEditScope, ConfigSource, ConfigStackOptions, RemoteConfigRefusal,
    RemoteConfigRemove, RemoteConfigSet, Repository, WorktreeConfig,
};

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

    assert_eq!(value.source, ConfigSource::Local { path: config_path });
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

#[test]
fn repository_remote_config_with_sources_reports_external_include_refusal() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let outside = temp.path.join("remotes.cfg");
    let repo = Repository::init(&repo_dir).expect("init");
    fs::write(
        &outside,
        b"[remote \"origin\"]\n\turl = old-url\n\tpushurl = push-url\n",
    )
    .expect("write include");
    fs::write(
        repo.common_dir().join("config"),
        format!("[include]\n\tpath = {}\n", outside.display()),
    )
    .expect("write config");

    let snapshot = repo.remote_config_with_sources().expect("remote snapshot");
    let origin = snapshot.get("origin").expect("origin remote");

    assert_eq!(origin.urls(), vec!["old-url"]);
    assert_eq!(origin.push_urls(), vec!["push-url"]);
    assert_eq!(origin.sources.len(), 1);
    assert!(!origin.sources[0].editable);
    assert_eq!(
        origin.sources[0].refusal,
        Some(RemoteConfigRefusal::ExternalInclude { path: outside })
    );
}

#[test]
fn repository_plan_remote_set_collapses_duplicate_sections_and_preserves_other_bytes() {
    let temp = TempDir::new();
    let repo = Repository::init(&temp.path).expect("init");
    let config_path = repo.common_dir().join("config");
    fs::write(
        &config_path,
        b"# keep\n[remote.origin] ; dotted\n\turl = old-one\n[remote \"origin\"] # quoted\n\turl = old-two\n\tfetch = old-fetch\n[core]\n\trepositoryformatversion = 0\n",
    )
    .expect("write config");

    let set = RemoteConfigSet::new("origin")
        .with_url("new-url")
        .with_push_url("push-url")
        .with_fetch_refspec("+refs/heads/*:refs/remotes/origin/*")
        .with_fetch_refspec("+refs/tags/*:refs/tags/*");
    let plan = repo
        .plan_remote_set(set, ConfigEditScope::Local)
        .expect("plan remote set");
    repo.apply_config_edit_plan(plan).expect("apply remote set");

    let updated = fs::read_to_string(&config_path).expect("read config");
    assert!(updated.contains("# keep\n"));
    assert!(updated.contains("[core]\n\trepositoryformatversion = 0\n"));
    assert!(!updated.contains("old-one"));
    assert!(!updated.contains("old-two"));
    assert!(!updated.contains("old-fetch"));
    assert_eq!(updated.matches("[remote \"origin\"]").count(), 1);
    assert!(updated.contains("\turl = new-url\n"));
    assert!(updated.contains("\tpushurl = push-url\n"));
    assert!(updated.contains("\tfetch = +refs/heads/*:refs/remotes/origin/*\n"));
    assert!(updated.contains("\tfetch = +refs/tags/*:refs/tags/*\n"));
}

#[test]
fn repository_plan_remote_remove_edits_included_file_inside_repo() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let repo = Repository::init(&repo_dir).expect("init");
    let included = repo_dir.join("remotes.cfg");
    fs::write(
        &included,
        b"[remote \"origin\"]\n\turl = old-one\n[remote.origin]\n\tfetch = old-fetch\n[remote \"backup\"]\n\turl = backup-url\n",
    )
    .expect("write include");
    fs::write(
        repo.common_dir().join("config"),
        format!("[include]\n\tpath = {}\n", included.display()),
    )
    .expect("write config");

    let plan = repo
        .plan_remote_remove(
            RemoteConfigRemove::new("origin"),
            ConfigEditScope::ExistingValue {
                allow_external_includes: false,
            },
        )
        .expect("included remote is editable");

    assert_eq!(plan.target_path, included);
    repo.apply_config_edit_plan(plan).expect("apply remove");

    assert_eq!(
        fs::read_to_string(repo.common_dir().join("config")).expect("read main"),
        format!("[include]\n\tpath = {}\n", included.display())
    );
    let updated = fs::read_to_string(&included).expect("read include");
    assert!(!updated.contains("origin"));
    assert!(!updated.contains("old-fetch"));
    assert!(updated.contains("[remote \"backup\"]\n\turl = backup-url\n"));
}

#[test]
fn repository_plan_remote_set_refuses_external_include_by_default() {
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

    let err = repo
        .plan_remote_set(
            RemoteConfigSet::new("origin").with_url("new-url"),
            ConfigEditScope::ExistingValue {
                allow_external_includes: false,
            },
        )
        .expect_err("external include is refused");

    assert!(matches!(
        err,
        ConfigEditError::RefusesExternalInclude { path } if path == outside
    ));
}

#[test]
fn repository_config_stack_reports_editable_remote_origin() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let repo = Repository::init(&repo_dir).expect("init");
    fs::write(
        repo.common_dir().join("config"),
        b"[remote \"origin\"]\n\turl = https://example.invalid/repo.git\n",
    )
    .expect("write config");

    let stack = repo
        .config_stack(ConfigStackOptions::git_default().worktree_config(WorktreeConfig::Never))
        .expect("config stack");
    let origin = stack.remote("origin").expect("origin remote");
    assert_eq!(origin.name(), "origin");
    assert_eq!(
        stack
            .editable_section_file(origin.section_id())
            .expect("editable file"),
        repo.common_dir().join("config")
    );
}

#[test]
fn repository_config_stack_includes_worktree_config_only_when_enabled() {
    let temp = TempDir::new();
    let repo_dir = temp.path.join("repo");
    let repo = Repository::init(&repo_dir).expect("init");
    fs::write(
        repo.git_dir().join("config.worktree"),
        b"[remote \"scratch\"]\n\turl = worktree-url\n",
    )
    .expect("write worktree config");

    let disabled = repo
        .config_stack(ConfigStackOptions::git_default())
        .expect("disabled stack");
    assert!(disabled.remote("scratch").is_err());

    fs::write(
        repo.common_dir().join("config"),
        b"[extensions]\n\tworktreeConfig = true\n",
    )
    .expect("enable worktree config");
    let enabled = repo
        .config_stack(ConfigStackOptions::git_default())
        .expect("enabled stack");
    let scratch = enabled.remote("scratch").expect("scratch remote");
    assert_eq!(
        enabled
            .editable_section_file(scratch.section_id())
            .expect("editable worktree file"),
        repo.git_dir().join("config.worktree")
    );
}
