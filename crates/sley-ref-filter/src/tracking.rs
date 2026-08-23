//! Upstream / push destination resolution and ahead-behind tracking.

use super::ForEachRefTrack;
use sley_config::GitConfig;
use sley_core::{ObjectId, ObjectFormat, Result};
use sley_odb::{FileObjectDatabase, ObjectReader};
use sley_refs::{FileRefStore, Ref, RefTarget, validate_ref_name};
use std::path::Path;

#[derive(Clone)]
pub struct ForEachRefUpstream {
    pub refname: String,
    pub remote: String,
    pub merge: String,
}

#[derive(Clone)]
pub struct ForEachRefPush {
    pub refname: Option<String>,
    pub remote: String,
    pub remote_ref: Option<String>,
}

pub struct ForEachRefPushRemote {
    name: String,
    expose_name: bool,
}

pub fn for_each_ref_upstream(
    config: &GitConfig,
    refname: &str,
) -> Option<ForEachRefUpstream> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let remote = config.get("branch", Some(branch), "remote")?;
    let merge = config.get("branch", Some(branch), "merge")?;
    if remote == "." {
        // git's `set_merge` for the local remote `.`: when fetch refspec mapping
        // fails, `repo_dwim_ref` expands a short merge name (e.g. `main`) to the
        // unique matching ref (`refs/heads/main`). Local-branch upstreams almost
        // always live under `refs/heads/`; fully-qualified `refs/*` values are
        // kept as-is.
        let refname = expand_local_upstream_merge(merge);
        return Some(ForEachRefUpstream {
            refname,
            remote: remote.to_string(),
            merge: merge.to_string(),
        });
    }
    let fetch = config.get("remote", Some(remote), "fetch")?;
    Some(ForEachRefUpstream {
        refname: map_remote_fetch_refspec(fetch, merge)?,
        remote: remote.to_string(),
        merge: merge.to_string(),
    })
}

/// Expand a loosely defined local `branch.<name>.merge` value the way git's
/// `set_merge` + `repo_dwim_ref` does for remote `.`.
pub fn expand_local_upstream_merge(merge: &str) -> String {
    if merge.starts_with("refs/") {
        merge.to_string()
    } else {
        format!("refs/heads/{merge}")
    }
}

pub fn for_each_ref_push(config: &GitConfig, refname: &str) -> Option<ForEachRefPush> {
    let branch = refname.strip_prefix("refs/heads/")?;
    let push_remote = for_each_ref_push_remote(config, branch)?;
    let remote_name = push_remote.name.clone();
    // The display name is exposed by `%(push:remotename)` even when the push
    // destination itself does not resolve, so compute it up front and keep it
    // on every return path (git's branch_get_push reports the remote regardless).
    let display_remote = remote_display_name(push_remote);
    if remote_name == "." {
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // An explicit push refspec (remote.<name>.push) takes precedence over
    // push.default — mirrors `remote->push.nr` in git's branch_get_push_1.
    if let Some(push) = config.get("remote", Some(remote_name.as_str()), "push") {
        if let Some(remote_ref) = map_remote_push_refspec(push, refname) {
            let tracking = map_remote_tracking_ref(config, &remote_name, &remote_ref);
            return Some(ForEachRefPush {
                refname: tracking,
                remote: display_remote,
                remote_ref: Some(remote_ref),
            });
        }
        return Some(ForEachRefPush {
            refname: None,
            remote: display_remote,
            remote_ref: None,
        });
    }
    // Otherwise resolve the destination through push.default, exactly as
    // git's branch_get_push_1 switch does.
    let push_default = config.get("push", None, "default").unwrap_or("simple");
    let tracking = match push_default {
        "nothing" => None,
        // matching/current push the branch's own ref through the push remote's
        // fetch refspec (tracking_for_push_dest on branch->refname).
        "matching" | "current" => map_remote_tracking_ref(config, &remote_name, refname),
        // upstream uses the branch's configured upstream destination.
        "upstream" => for_each_ref_upstream(config, refname).map(|up| up.refname),
        // simple/unspecified (the default): the push destination must equal the
        // upstream destination, otherwise there is no single 'simple' target and
        // %(push) is empty (the remote name is still reported).
        _ => {
            let up = for_each_ref_upstream(config, refname).map(|up| up.refname);
            let cur = map_remote_tracking_ref(config, &remote_name, refname);
            match (up, cur) {
                (Some(up), Some(cur)) if up == cur => Some(cur),
                _ => None,
            }
        }
    };
    Some(ForEachRefPush {
        refname: tracking,
        remote: display_remote,
        remote_ref: None,
    })
}

pub fn for_each_ref_push_remote(
    config: &GitConfig,
    branch: &str,
) -> Option<ForEachRefPushRemote> {
    if let Some(remote) = config.get("branch", Some(branch), "pushRemote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("remote", None, "pushDefault") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if let Some(remote) = config.get("branch", Some(branch), "remote") {
        return Some(ForEachRefPushRemote {
            name: remote.to_string(),
            expose_name: true,
        });
    }
    if sley_config::remotes::remote_exists(config, "origin") {
        return Some(ForEachRefPushRemote {
            name: "origin".to_string(),
            expose_name: false,
        });
    }
    let remotes = sley_config::remotes::remote_names(config);
    match remotes.as_slice() {
        [remote] => Some(ForEachRefPushRemote {
            name: remote.clone(),
            expose_name: false,
        }),
        _ => None,
    }
}

pub fn remote_display_name(remote: ForEachRefPushRemote) -> String {
    if remote.expose_name {
        remote.name
    } else {
        String::new()
    }
}

pub fn map_remote_tracking_ref(
    config: &GitConfig,
    remote: &str,
    remote_ref: &str,
) -> Option<String> {
    let fetch = config.get("remote", Some(remote), "fetch")?;
    map_remote_fetch_refspec(fetch, remote_ref)
}

pub fn map_remote_push_refspec(refspec: &str, refname: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.src.is_none() || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, refname).ok()?
}

pub fn map_remote_fetch_refspec(refspec: &str, merge: &str) -> Option<String> {
    let refspec = parse_refspec(refspec).ok()?;
    if refspec.negative || refspec.dst.is_none() {
        return None;
    }
    refspec_map_source(&refspec, merge).ok()?
}

use sley_protocol::parse_refspec;
use sley_protocol::refspec_map_source;

pub fn for_each_ref_upstream_track(
    store: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    upstream: &str,
) -> Result<Option<ForEachRefTrack>> {
    // git: a configured-but-unresolvable upstream reports `[gone]`, distinct
    // from "no upstream configured" (which the caller already filtered out).
    let gone_track = ForEachRefTrack {
        ahead: 0,
        behind: 0,
        gone: true,
    };
    let Some(upstream_target) = store.read_ref(upstream)? else {
        return Ok(Some(gone_track));
    };
    let upstream_ref = Ref {
        name: upstream.to_string(),
        target: upstream_target,
    };
    let Some((upstream_oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)? else {
        return Ok(Some(gone_track));
    };
    for_each_ref_ahead_behind(git_dir, db, format, oid, &upstream_oid)
}

pub fn for_each_ref_ahead_behind_with_diagnostic(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        if let Ok(object) = db.read_object(oid) {
            eprintln!(
                "error: object {} is a {}, not a commit",
                oid,
                object.object_type.as_str()
            );
        }
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

pub fn for_each_ref_ahead_behind(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ForEachRefTrack>> {
    let Ok(local_commit) = sley_rev::peel_to_commit(db, format, oid) else {
        return Ok(None);
    };
    let Ok(target_commit) = sley_rev::peel_to_commit(db, format, target) else {
        return Ok(None);
    };
    let (ahead, behind) =
        sley_rev::ahead_behind_counts(git_dir, format, db, &local_commit, &target_commit)?;
    Ok(Some(ForEachRefTrack {
        ahead,
        behind,
        gone: false,
    }))
}

pub fn resolve_for_each_ref_target(
    store: &FileRefStore,
    reference: &Ref,
) -> Result<Option<(ObjectId, Option<String>)>> {
    let mut target = reference.target.clone();
    let mut symref = None;
    for _ in 0..5 {
        match target {
            RefTarget::Direct(oid) => return Ok(Some((oid, symref))),
            RefTarget::Symbolic(name) => {
                symref.get_or_insert_with(|| name.clone());
                if validate_ref_name(&name).is_err() {
                    return Ok(None);
                }
                let Some(next) = store.read_ref(&name)? else {
                    return Ok(None);
                };
                target = next;
            }
        }
    }
    Ok(None)
}
