//! Typed repository administration for configured remotes.
//!
//! Argument parsing and byte rendering belong to the CLI. This module owns the
//! reusable configuration plans and ref transactions behind `git remote`.

use std::collections::BTreeSet;
use std::mem;

use sley_config::{ConfigEntry, ConfigSection, GitConfig};
use sley_core::Result;
use sley_refs::{FileRefStore, RefTarget, RefUpdate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMirror {
    None,
    Fetch,
    Push,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTagMode {
    Default,
    All,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRemoteOptions {
    pub name: String,
    pub url: String,
    pub branches: Vec<String>,
    pub master: Option<String>,
    pub tags: RemoteTagMode,
    pub mirror: RemoteMirror,
    pub fetch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddRemoteOutcome {
    pub fetch: bool,
    pub master_head: Option<RemoteHeadPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAdminError {
    AlreadyExists,
    NotFound,
    NameSubset { existing: String },
    NameSuperset { existing: String },
    MasterWithMirror,
    TrackingWithPushMirror,
    RenameCollision,
}

/// Add a complete remote section and return post-config work as typed plans.
pub fn add_remote(
    config: &mut GitConfig,
    options: &AddRemoteOptions,
) -> std::result::Result<AddRemoteOutcome, RemoteAdminError> {
    if options.mirror != RemoteMirror::None && options.master.is_some() {
        return Err(RemoteAdminError::MasterWithMirror);
    }
    if options.mirror == RemoteMirror::Push && !options.branches.is_empty() {
        return Err(RemoteAdminError::TrackingWithPushMirror);
    }
    for existing in sley_config::remotes::remote_names(config) {
        if let Some(rest) = options.name.strip_prefix(&existing)
            && rest.starts_with('/')
        {
            return Err(RemoteAdminError::NameSubset { existing });
        }
        if let Some(rest) = existing.strip_prefix(&options.name)
            && rest.starts_with('/')
        {
            return Err(RemoteAdminError::NameSuperset { existing });
        }
    }
    let mut entries = vec![ConfigEntry::new("url", Some(options.url.clone()))];
    match options.mirror {
        RemoteMirror::Fetch | RemoteMirror::Both => {
            if options.branches.is_empty() {
                entries.push(ConfigEntry::new("fetch", Some("+refs/*:refs/*".into())));
            } else {
                for branch in &options.branches {
                    entries.push(ConfigEntry::new(
                        "fetch",
                        Some(mirror_fetch_refspec(branch)),
                    ));
                }
            }
        }
        RemoteMirror::Push => entries.push(ConfigEntry::new("mirror", Some("true".into()))),
        RemoteMirror::None => {
            if options.branches.is_empty() {
                entries.push(ConfigEntry::new(
                    "fetch",
                    Some(sley_config::remotes::default_fetch_refspec(&options.name)),
                ));
            } else {
                for branch in &options.branches {
                    entries.push(ConfigEntry::new(
                        "fetch",
                        Some(remote_branch_fetch_refspec(&options.name, branch)),
                    ));
                }
            }
        }
    }
    match options.tags {
        RemoteTagMode::Default => {}
        RemoteTagMode::All => entries.push(ConfigEntry::new("tagOpt", Some("--tags".into()))),
        RemoteTagMode::None => entries.push(ConfigEntry::new("tagOpt", Some("--no-tags".into()))),
    }
    if options.mirror == RemoteMirror::Both {
        entries.push(ConfigEntry::new("mirror", Some("true".into())));
    }
    sley_config::remotes::add_remote(config, &options.name, entries).map_err(
        |error| match error {
            sley_config::remotes::RemoteEditError::AlreadyExists => RemoteAdminError::AlreadyExists,
            sley_config::remotes::RemoteEditError::NotFound => RemoteAdminError::NotFound,
        },
    )?;
    Ok(AddRemoteOutcome {
        fetch: options.fetch,
        master_head: options
            .master
            .as_ref()
            .map(|branch| RemoteHeadPlan::set(&options.name, branch)),
    })
}

fn mirror_fetch_refspec(branch: &str) -> String {
    if branch == "*" {
        "+refs/*:refs/*".to_string()
    } else {
        format!("+refs/{branch}:refs/{branch}")
    }
}

pub fn remote_branch_fetch_refspec(remote: &str, branch: &str) -> String {
    format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveRemoteOutcome {
    pub warn_local_branches: bool,
    pub tracking_prefix: String,
}

pub fn remove_remote(
    config: &mut GitConfig,
    name: &str,
) -> std::result::Result<RemoveRemoteOutcome, RemoteAdminError> {
    let warn_local_branches = sley_config::remotes::remote_config_values(config, name, "fetch")
        .iter()
        .filter_map(|refspec| refspec.rsplit_once(':').map(|(_, destination)| destination))
        .any(|destination| destination == "refs/*" || destination == "+refs/*");
    sley_config::remotes::remove_remote(config, name).map_err(|error| match error {
        sley_config::remotes::RemoteEditError::NotFound => RemoteAdminError::NotFound,
        sley_config::remotes::RemoteEditError::AlreadyExists => RemoteAdminError::AlreadyExists,
    })?;
    Ok(RemoveRemoteOutcome {
        warn_local_branches,
        tracking_prefix: format!("refs/remotes/{name}/"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameRemoteOutcome {
    pub rename_tracking_refs: bool,
}

pub fn rename_remote(
    config: &mut GitConfig,
    old: &str,
    new: &str,
) -> std::result::Result<RenameRemoteOutcome, RemoteAdminError> {
    let old_exists = sley_config::remotes::remote_exists(config, old);
    if !old_exists {
        return Err(RemoteAdminError::NotFound);
    }
    if old != new && sley_config::remotes::remote_exists(config, new) {
        return Err(RemoteAdminError::RenameCollision);
    }
    let rename_tracking_refs = sley_config::remotes::remote_config_values(config, old, "fetch")
        .iter()
        .any(|refspec| fetch_refspec_targets_remote_tracking(refspec, old));
    if old == new {
        return Ok(RenameRemoteOutcome {
            rename_tracking_refs: false,
        });
    }
    for section in &mut config.sections {
        if section.name == "remote" && section.subsection.as_deref() == Some(old) {
            section.subsection = Some(new.to_string());
            for entry in &mut section.entries {
                if entry.key.eq_ignore_ascii_case("fetch")
                    && let Some(value) = &mut entry.value
                {
                    *value = value.replace(
                        &format!("refs/remotes/{old}/"),
                        &format!("refs/remotes/{new}/"),
                    );
                }
            }
            move_fetch_entries_to_end(section);
        } else if section.name == "branch" {
            for entry in &mut section.entries {
                if (entry.key.eq_ignore_ascii_case("remote")
                    || entry.key.eq_ignore_ascii_case("pushRemote"))
                    && entry.value.as_deref() == Some(old)
                {
                    entry.value = Some(new.to_string());
                }
            }
        } else if section.name == "remote" && section.subsection.is_none() {
            for entry in &mut section.entries {
                if entry.key.eq_ignore_ascii_case("pushDefault")
                    && entry.value.as_deref() == Some(old)
                {
                    entry.value = Some(new.to_string());
                }
            }
        }
    }
    Ok(RenameRemoteOutcome {
        rename_tracking_refs,
    })
}

fn move_fetch_entries_to_end(section: &mut ConfigSection) {
    let mut fetch_entries = Vec::new();
    let entries = mem::take(&mut section.entries);
    for entry in entries {
        if entry.key.eq_ignore_ascii_case("fetch") {
            fetch_entries.push(entry);
        } else {
            section.entries.push(entry);
        }
    }
    section.entries.extend(fetch_entries);
}

fn fetch_refspec_targets_remote_tracking(refspec: &str, remote: &str) -> bool {
    let Some((_, destination)) = refspec.strip_prefix('+').unwrap_or(refspec).split_once(':')
    else {
        return false;
    };
    destination.starts_with(&format!("refs/remotes/{remote}/"))
}

pub fn set_remote_branches(
    config: &mut GitConfig,
    name: &str,
    branches: &[String],
    add: bool,
) -> std::result::Result<(), RemoteAdminError> {
    let mirror_fetch = sley_config::remotes::remote_config_values(config, name, "fetch")
        .iter()
        .any(|value| value == "+refs/*:refs/*");
    let Some(section) =
        config.sections.iter_mut().rev().find(|section| {
            section.name == "remote" && section.subsection.as_deref() == Some(name)
        })
    else {
        return Err(RemoteAdminError::NotFound);
    };
    if !add {
        section
            .entries
            .retain(|entry| !entry.key.eq_ignore_ascii_case("fetch"));
    }
    for branch in branches {
        section.entries.push(ConfigEntry::new(
            "fetch",
            Some(if mirror_fetch {
                mirror_fetch_refspec(branch)
            } else {
                remote_branch_fetch_refspec(name, branch)
            }),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHeadMutation {
    Delete,
    Set { target: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHeadPlan {
    pub head: String,
    pub mutation: RemoteHeadMutation,
}

impl RemoteHeadPlan {
    pub fn set(remote: &str, branch: &str) -> Self {
        Self {
            head: format!("refs/remotes/{remote}/HEAD"),
            mutation: RemoteHeadMutation::Set {
                target: format!("refs/remotes/{remote}/{branch}"),
            },
        }
    }

    pub fn delete(remote: &str) -> Self {
        Self {
            head: format!("refs/remotes/{remote}/HEAD"),
            mutation: RemoteHeadMutation::Delete,
        }
    }
}

pub fn apply_remote_head(store: &FileRefStore, plan: &RemoteHeadPlan) -> Result<()> {
    match &plan.mutation {
        RemoteHeadMutation::Delete => {
            let _ = store.delete_symbolic_ref(&plan.head)?;
            Ok(())
        }
        RemoteHeadMutation::Set { target } => {
            let mut transaction = store.transaction();
            transaction.update(RefUpdate {
                name: plan.head.clone(),
                expected: None,
                new: RefTarget::Symbolic(target.clone()),
                reflog: None,
            });
            transaction.commit()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePrunePlan {
    pub remote: String,
    pub stale_refs: Vec<String>,
    pub head_target: Option<String>,
}

pub fn plan_remote_prune(
    config: &GitConfig,
    remote: &str,
    remote_refs: &[sley_refs::Ref],
    local_refs: &[sley_refs::Ref],
    head_target: Option<String>,
) -> RemotePrunePlan {
    let mut stale = BTreeSet::new();
    for spec in sley_config::remotes::remote_config_values(config, remote, "fetch") {
        let Some((source_prefix, destination_prefix)) = fetch_refspec_prefixes(&spec) else {
            continue;
        };
        let remote_names = branch_names_with_prefix(remote_refs, source_prefix)
            .into_iter()
            .collect::<BTreeSet<_>>();
        for suffix in branch_names_with_prefix(local_refs, destination_prefix) {
            if !remote_names.contains(&suffix) {
                stale.insert(format!("{destination_prefix}{suffix}"));
            }
        }
    }
    RemotePrunePlan {
        remote: remote.to_string(),
        stale_refs: stale.into_iter().collect(),
        head_target,
    }
}

fn fetch_refspec_prefixes(spec: &str) -> Option<(&str, &str)> {
    if spec.starts_with('^') {
        return None;
    }
    let spec = spec.strip_prefix('+').unwrap_or(spec);
    let (source, destination) = spec.split_once(':')?;
    if source == "refs/*" && destination == "refs/*" {
        return Some(("refs/heads/", "refs/heads/"));
    }
    Some((source.strip_suffix('*')?, destination.strip_suffix('*')?))
}

fn branch_names_with_prefix(refs: &[sley_refs::Ref], prefix: &str) -> Vec<String> {
    refs.iter()
        .filter_map(|reference| reference.name.strip_prefix(prefix))
        .filter(|branch| *branch != "HEAD")
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(input: &str) -> GitConfig {
        GitConfig::parse(input.as_bytes()).expect("parse config")
    }

    #[test]
    fn add_plan_builds_default_fetch_and_master_head() {
        let mut config = GitConfig::default();
        let outcome = add_remote(
            &mut config,
            &AddRemoteOptions {
                name: "origin".into(),
                url: "../remote".into(),
                branches: Vec::new(),
                master: Some("main".into()),
                tags: RemoteTagMode::Default,
                mirror: RemoteMirror::None,
                fetch: false,
            },
        )
        .expect("add remote");
        assert_eq!(
            sley_config::remotes::remote_config_values(&config, "origin", "fetch"),
            vec!["+refs/heads/*:refs/remotes/origin/*"]
        );
        assert_eq!(
            outcome.master_head,
            Some(RemoteHeadPlan::set("origin", "main"))
        );
    }

    #[test]
    fn rename_plan_updates_remote_and_branch_backreferences() {
        let mut config = config(
            "[remote \"old\"]\n\turl = x\n\tfetch = +refs/heads/*:refs/remotes/old/*\n\
             [branch \"main\"]\n\tremote = old\n\tmerge = refs/heads/main\n",
        );
        let outcome = rename_remote(&mut config, "old", "new").expect("rename remote");
        assert!(outcome.rename_tracking_refs);
        assert!(sley_config::remotes::remote_exists(&config, "new"));
        assert_eq!(config.get("branch", Some("main"), "remote"), Some("new"));
    }

    #[test]
    fn prune_plan_maps_fetch_refspecs_without_filesystem_access() {
        use sley_core::{ObjectFormat, ObjectId};

        let oid = ObjectId::null(ObjectFormat::Sha1);
        let config = config("[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n");
        let remote_refs = vec![sley_refs::Ref {
            name: "refs/heads/main".into(),
            target: RefTarget::Direct(oid),
        }];
        let local_refs = vec![
            sley_refs::Ref {
                name: "refs/remotes/origin/main".into(),
                target: RefTarget::Direct(oid),
            },
            sley_refs::Ref {
                name: "refs/remotes/origin/gone".into(),
                target: RefTarget::Direct(oid),
            },
            sley_refs::Ref {
                name: "refs/remotes/origin/HEAD".into(),
                target: RefTarget::Symbolic("refs/remotes/origin/main".into()),
            },
        ];
        let plan = plan_remote_prune(&config, "origin", &remote_refs, &local_refs, None);
        assert_eq!(plan.stale_refs, vec!["refs/remotes/origin/gone"]);
    }
}
