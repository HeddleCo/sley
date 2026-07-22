//! Branch upstream (tracking) configuration.

use super::config::{remove_branch_config, write_branch_repo_config};
use super::operand::{BranchOperandKind, branch_resolve_local_branch_operand};
use crate::*;
use sley::plumbing::{sley_refs, sley_rev};

pub(super) enum BranchUpstreamAction {
    Set(String),
    Unset,
}

pub(super) struct BranchUpstreamOptions {
    pub(crate) action: BranchUpstreamAction,
    pub(crate) branches: Vec<String>,
}

#[rustfmt::skip]
pub(super) fn run_branch_upstream_options(
    git_dir: &Path,
    store: &FileRefStore,
    replace_objects: bool,
    options: BranchUpstreamOptions,
) -> Result<()> {
    let format = repository_object_format(git_dir)?;
    match options.action {
        BranchUpstreamAction::Set(upstream) => {
            if options.branches.len() > 1 {
                eprintln!("fatal: too many arguments to set new upstream");
                return Err(GitError::Exit(128));
            }
            let upstream = branch_upstream_resolve_previous_checkout(git_dir, &upstream)?;
            let branch = branch_upstream_target_branch(
                git_dir,
                format,
                store,
                options.branches.first(),
                true,
                &upstream,
            )?;
            set_branch_upstream(git_dir, store, replace_objects, &branch, &upstream)
        }
        BranchUpstreamAction::Unset => {
            if options.branches.len() > 1 {
                eprintln!("fatal: too many arguments to unset upstream");
                return Err(GitError::Exit(128));
            }
            let branch = branch_upstream_target_branch(
                git_dir,
                format,
                store,
                options.branches.first(),
                false,
                "",
            )?;
            unset_branch_upstream(git_dir, &branch)
        }
    }
}

pub(super) fn branch_upstream_resolve_previous_checkout(
    git_dir: &Path,
    upstream: &str,
) -> Result<String> {
    let Some(inner) = upstream
        .strip_prefix("@{-")
        .and_then(|value| value.strip_suffix('}'))
    else {
        return Ok(upstream.to_string());
    };
    let n = inner
        .parse::<usize>()
        .map_err(|_| GitError::InvalidFormat(format!("invalid branch name: '{upstream}'")))?;
    let format = repository_object_format(git_dir)?;
    Ok(
        sley_rev::nth_prior_checkout_branch_name(git_dir, format, n)?
            .unwrap_or_else(|| upstream.to_string()),
    )
}

pub(super) fn branch_upstream_target_branch(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    explicit: Option<&String>,
    setting: bool,
    upstream: &str,
) -> Result<String> {
    if let Some(branch) = explicit {
        let (branch, refname) =
            match branch_resolve_upstream_target_branch_operand(git_dir, format, store, branch) {
                Ok(resolved) => resolved,
                Err(GitError::InvalidPath(_)) => {
                    branch_upstream_missing_branch(branch, setting);
                    return Err(GitError::Exit(128));
                }
                Err(err) => return Err(err),
            };
        if store.read_ref(&refname)?.is_none() {
            if setting {
                eprintln!("fatal: branch '{branch}' does not exist");
            } else {
                eprintln!("fatal: branch '{branch}' has no upstream information");
            }
            return Err(GitError::Exit(128));
        }
        return Ok(branch);
    }
    let Some(branch) = store.current_branch()? else {
        if setting {
            eprintln!(
                "fatal: could not set upstream of HEAD to {upstream} when it does not point to any branch"
            );
        } else {
            eprintln!(
                "fatal: could not unset upstream of HEAD when it does not point to any branch"
            );
        }
        return Err(GitError::Exit(128));
    };
    Ok(branch)
}

pub(super) fn branch_resolve_upstream_target_branch_operand(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: &str,
) -> Result<(String, String)> {
    if branch.contains("@{") {
        return branch_resolve_local_branch_operand(
            git_dir,
            format,
            store,
            branch,
            BranchOperandKind::Existing,
        );
    }
    match sley_refs::branch_ref_name_for_source(branch) {
        Ok(refname) => Ok((branch.to_string(), refname)),
        Err(GitError::InvalidPath(_)) => Err(GitError::InvalidPath(branch.to_string())),
        Err(err) => Err(err),
    }
}

pub(super) fn branch_upstream_missing_branch(branch: &str, setting: bool) {
    if setting {
        eprintln!("fatal: branch '{branch}' does not exist");
    } else {
        eprintln!("fatal: branch '{branch}' has no upstream information");
    }
}
pub(super) fn set_branch_upstream(
    git_dir: &Path,
    store: &FileRefStore,
    replace_objects: bool,
    branch: &str,
    upstream: &str,
) -> Result<()> {
    // Effective config for resolution (sees `-c`); on-disk for the write-back so
    // command-line injections are never persisted (git keeps `-c` process-local).
    let effective = read_repo_config(git_dir)?;
    let format = repository_object_format(git_dir)?;
    if branch_upstream_is_non_ref(git_dir, format, replace_objects, upstream)? {
        eprintln!(
            "fatal: cannot set up tracking information; starting point '{upstream}' is not a branch"
        );
        return Err(GitError::Exit(128));
    }
    let Some(upstream) = resolve_branch_upstream(git_dir, format, store, &effective, upstream)?
    else {
        eprintln!("fatal: the requested upstream branch '{upstream}' does not exist");
        eprintln!("hint:");
        eprintln!("hint: If you are planning on basing your work on an upstream");
        eprintln!("hint: branch that already exists at the remote, you may need to");
        eprintln!("hint: run \"git fetch\" to retrieve it.");
        eprintln!("hint:");
        eprintln!("hint: If you are planning to push out a new local branch that");
        eprintln!("hint: will track its remote counterpart, you may want to use");
        eprintln!("hint: \"git push -u\" to set the upstream config as you push.");
        eprintln!(
            "hint: Disable this message with \"git config set advice.setUpstreamFailure false\""
        );
        return Err(GitError::Exit(128));
    };
    let branch_ref = branch_ref_name(branch)?;
    if upstream.remote == "." && upstream.merge == branch_ref {
        eprintln!("warning: not setting branch '{branch}' as its own upstream");
        return Ok(());
    }
    let mut config = read_repo_config_on_disk(git_dir)?;
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "remote",
        &upstream.remote,
    );
    set_config_value(
        &mut config,
        "branch",
        Some(branch),
        "merge",
        &upstream.merge,
    );
    write_branch_repo_config(git_dir, &config)?;
    println!("branch '{branch}' set up to track '{}'.", upstream.display);
    Ok(())
}

pub(super) fn branch_upstream_is_non_ref(
    git_dir: &Path,
    format: ObjectFormat,
    replace_objects: bool,
    upstream: &str,
) -> Result<bool> {
    if sley_rev::resolve_revision_symbolic_full_name(git_dir, format, upstream)
        .ok()
        .flatten()
        .is_some()
    {
        return Ok(false);
    }
    Ok(resolve_revision(git_dir, format, upstream, replace_objects).is_ok())
}

pub(super) struct ResolvedBranchUpstream {
    pub(crate) remote: String,
    pub(crate) merge: String,
    pub(crate) display: String,
}

pub(super) fn resolve_branch_upstream(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    upstream: &str,
) -> Result<Option<ResolvedBranchUpstream>> {
    let resolved_upstream = if upstream.contains("@{") {
        sley_rev::resolve_revision_symbolic_full_name(git_dir, format, upstream)
            .ok()
            .flatten()
    } else {
        None
    };
    let upstream = resolved_upstream.as_deref().unwrap_or(upstream);
    let local_branch = upstream.strip_prefix("refs/heads/").unwrap_or(upstream);
    if let Ok(local_ref) = branch_ref_name(local_branch)
        && store.read_ref(&local_ref)?.is_some()
    {
        return Ok(Some(ResolvedBranchUpstream {
            remote: ".".into(),
            merge: local_ref,
            display: local_branch.to_string(),
        }));
    }
    for remote in remote_names(config) {
        let Some((remote_ref, merge)) = branch_upstream_remote_ref(config, &remote, upstream)
        else {
            continue;
        };
        if store.read_ref(&remote_ref)?.is_some() {
            let display = remote_ref
                .strip_prefix("refs/remotes/")
                .unwrap_or(remote_ref.as_str())
                .to_string();
            return Ok(Some(ResolvedBranchUpstream {
                remote,
                merge,
                display,
            }));
        }
    }
    Ok(None)
}

pub(super) fn branch_upstream_remote_ref(
    config: &GitConfig,
    remote: &str,
    upstream: &str,
) -> Option<(String, String)> {
    let remote_ref = branch_remote_ref_candidate(remote, upstream)?;
    for fetch in config
        .get_all("remote", Some(remote), "fetch")
        .into_iter()
        .flatten()
    {
        let refspec = parse_refspec(fetch).ok()?;
        if refspec.negative {
            continue;
        }
        let dst = refspec.dst.as_deref()?;
        let src = refspec.src.as_deref()?;
        if refspec.pattern {
            let (dst_prefix, dst_suffix) = dst.split_once('*')?;
            let Some(middle) = remote_ref
                .strip_prefix(dst_prefix)
                .and_then(|value| value.strip_suffix(dst_suffix))
            else {
                continue;
            };
            let (src_prefix, src_suffix) = src.split_once('*')?;
            let merge = format!("{src_prefix}{middle}{src_suffix}");
            return Some((remote_ref, merge));
        }
        if dst == remote_ref {
            return Some((remote_ref, src.to_string()));
        }
    }
    None
}

pub(super) fn branch_remote_ref_candidate(remote: &str, upstream: &str) -> Option<String> {
    if upstream.starts_with("refs/") {
        return Some(upstream.to_string());
    }
    if let Some(name) = upstream.strip_prefix("remotes/") {
        return Some(format!("refs/remotes/{name}"));
    }
    if let Some(branch) = upstream.strip_prefix(&format!("{remote}/")) {
        return Some(format!("refs/remotes/{remote}/{branch}"));
    }
    Some(branch_tracking_ref_candidate(upstream))
}

pub(super) fn branch_tracking_ref_candidate(upstream: &str) -> String {
    if upstream.starts_with("refs/") {
        upstream.to_string()
    } else if let Some(name) = upstream.strip_prefix("remotes/") {
        format!("refs/remotes/{name}")
    } else {
        format!("refs/heads/{upstream}")
    }
}

pub(super) fn unset_branch_upstream(git_dir: &Path, branch: &str) -> Result<()> {
    let mut config = read_repo_config_on_disk(git_dir)?;
    let Some(section_idx) = config.sections.iter().rposition(|section| {
        section.name == "branch" && section.subsection.as_deref() == Some(branch)
    }) else {
        eprintln!("fatal: branch '{branch}' has no upstream information");
        return Err(GitError::Exit(128));
    };
    let had_upstream = {
        let section = &mut config.sections[section_idx];
        let before = section.entries.len();
        section
            .entries
            .retain(|entry| !matches!(entry.key.as_str(), "remote" | "merge"));
        section.entries.len() != before
    };
    if !had_upstream {
        eprintln!("fatal: branch '{branch}' has no upstream information");
        return Err(GitError::Exit(128));
    }
    config
        .sections
        .retain(|section| !(section.name == "branch" && section.entries.is_empty()));
    write_branch_repo_config(git_dir, &config)
}
