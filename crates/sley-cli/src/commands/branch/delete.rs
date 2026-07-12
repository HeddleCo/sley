//! Branch deletion (merged, force, remote-tracking).

use super::config::remove_branch_config;
use super::create::{
    branch_create_reflog_message, branch_reset_reflog_message, branch_should_write_reflog,
    resolve_branch_start,
};
use super::operand::{BranchOperandKind, branch_resolve_local_branch_operand};
use crate::*;

pub(super) struct BranchDeleteOptions {
    pub(crate) force: bool,
    pub(crate) quiet: bool,
    pub(crate) mode: BranchDeleteMode,
    pub(crate) branches: Vec<String>,
}

#[derive(Clone, Copy)]
pub(super) enum BranchDeleteMode {
    Local,
    Remote,
    All,
}

/// If `name` is a symbolic ref, delete the symref itself (not its target),
/// printing git's `Deleted branch <branch> (was <raw-target>).` message and
/// returning `Ok(Some(()))`. Mirrors builtin/branch.c, which resolves the
/// branch with `RESOLVE_REF_NO_RECURSE`, so the merge check is bypassed and the
/// reported value is the symref's immediate target verbatim (t3200 #81-#83).
pub(super) fn try_delete_symref_branch(
    store: &FileRefStore,
    name: &str,
    branch: &str,
    quiet: bool,
) -> Result<Option<()>> {
    let Some(RefTarget::Symbolic(target)) = store.read_ref(name)? else {
        return Ok(None);
    };
    store.delete_symbolic_ref(name)?;
    if !quiet {
        println!("Deleted branch {branch} (was {target}).");
    }
    Ok(Some(()))
}

pub(super) fn branch_checked_out_worktree_path(
    git_dir: &Path,
    _store: &FileRefStore,
    refname: &str,
) -> Result<Option<String>> {
    if let Some(worktree) = sley_worktree::find_shared_symref(git_dir, "HEAD", refname)? {
        return Ok(Some(worktree.path.to_string_lossy().into_owned()));
    }
    Ok(
        sley_worktree::worktree_holding_rebase_update_ref(git_dir, refname)?
            .map(|worktree| worktree.path.to_string_lossy().into_owned()),
    )
}

pub(super) fn force_delete_branches(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }
    let mut failed = false;
    for branch in branches {
        let (branch, name) =
            branch_delete_resolve_local_branch_arg(git_dir, format, store, branch)?;
        if store.read_ref(&name)?.is_none() {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        }
        if let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &name)? {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root
            );
            failed = true;
            continue;
        }
        if try_delete_symref_branch(store, &name, &branch, quiet)?.is_some() {
            continue;
        }
        let deleted_oid = delete_branch_ref(git_dir, store, &branch, &name)?;
        remove_branch_config(git_dir, &branch)?;
        if !quiet {
            let deleted_display = branch_delete_display(&branch, &name, &deleted_oid);
            println!("Deleted branch {branch} (was {deleted_display}).");
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

pub(super) fn branch_delete_resolve_local_branch_arg(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branch: &str,
) -> Result<(String, String)> {
    if branch.contains("@{")
        && let Ok(Some(refname)) =
            sley_rev::resolve_revision_symbolic_full_name(git_dir, format, branch)
        && let Some(local) = refname.strip_prefix("refs/heads/")
        && store.read_ref(&refname)?.is_some()
    {
        return Ok((local.to_string(), refname));
    }
    Ok((branch.to_string(), format!("refs/heads/{branch}")))
}

pub(super) fn delete_remote_tracking_branches(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }
    let mut failed = false;
    for branch in branches {
        let (branch, name) = branch_delete_resolve_remote_branch_arg(git_dir, format, branch)?;
        let Some(RefTarget::Direct(_)) = store.read_ref(&name)? else {
            eprintln!("error: remote-tracking branch '{branch}' not found");
            failed = true;
            continue;
        };
        let deleted = store.delete_ref(&name)?;
        if !quiet {
            println!(
                "Deleted remote-tracking branch {branch} (was {}).",
                short_oid(&deleted.oid.to_hex())
            );
        }
    }
    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

pub(super) fn branch_delete_resolve_remote_branch_arg(
    git_dir: &Path,
    format: ObjectFormat,
    branch: &str,
) -> Result<(String, String)> {
    if branch.contains("@{") {
        let Some(refname) = sley_rev::resolve_revision_symbolic_full_name(git_dir, format, branch)?
        else {
            eprintln!("error: remote-tracking branch '{branch}' not found");
            return Err(GitError::Exit(1));
        };
        let Some(remote) = refname.strip_prefix("refs/remotes/") else {
            eprintln!("error: remote-tracking branch '{branch}' not found");
            return Err(GitError::Exit(1));
        };
        return Ok((remote.to_string(), refname));
    }
    Ok((branch.to_string(), format!("refs/remotes/{branch}")))
}

pub(super) fn force_update_branch(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    config: &GitConfig,
    replace_objects: bool,
    branch: &str,
    start: Option<&String>,
) -> Result<String> {
    let (branch, name) = branch_resolve_local_branch_operand(
        git_dir,
        format,
        store,
        branch,
        BranchOperandKind::UpdateOrCreate,
    )?;
    if let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &name)? {
        eprintln!(
            "fatal: cannot force update the branch '{branch}' used by worktree at '{}'",
            worktree_root
        );
        return Err(GitError::Exit(128));
    }
    let start_rev = start.map_or("HEAD", String::as_str);
    let new_oid = resolve_branch_start(git_dir, format, store, replace_objects, start_rev)?;
    let previous = store.read_ref(&name)?;
    let reflog = match previous {
        Some(RefTarget::Direct(old_oid)) if old_oid == new_oid => None,
        Some(RefTarget::Direct(old_oid)) if branch_should_write_reflog(git_dir, &name, false)? => {
            Some(ReflogEntry {
                old_oid,
                new_oid,
                committer: commit_identity_from_env("COMMITTER", config)?,
                message: branch_reset_reflog_message(store, start)?,
            })
        }
        Some(_) if branch_should_write_reflog(git_dir, &name, false)? => Some(ReflogEntry {
            old_oid: zero_oid(format)?,
            new_oid,
            committer: commit_identity_from_env("COMMITTER", config)?,
            message: branch_reset_reflog_message(store, start)?,
        }),
        None if branch_should_write_reflog(git_dir, &name, false)? => Some(ReflogEntry {
            old_oid: ObjectId::null(format),
            new_oid,
            committer: commit_identity_from_env("COMMITTER", config)?,
            message: branch_create_reflog_message(store, start)?,
        }),
        _ => None,
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name,
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog,
    });
    tx.commit()?;
    Ok(branch)
}

pub(super) fn delete_merged_branches(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    store: &FileRefStore,
    replace_objects: bool,
    branches: &[String],
    quiet: bool,
) -> Result<()> {
    if branches.is_empty() {
        eprintln!("fatal: branch name required");
        return Err(GitError::Exit(128));
    }

    let config = read_repo_config(git_dir)?;
    let head_reachable = resolve_revision(git_dir, format, "HEAD", replace_objects)
        .ok()
        .and_then(|head| sley_rev::peel_to_commit(db, format, &head).ok())
        .map(|head| sley_rev::reachable_commit_oids(git_dir, format, db, [head], false))
        .transpose()?;

    let mut failed = false;
    for branch in branches {
        let (branch, name) =
            branch_delete_resolve_local_branch_arg(git_dir, format, store, branch)?;
        let Some(target) = store.read_ref(&name)? else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        if let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &name)? {
            eprintln!(
                "error: cannot delete branch '{branch}' used by worktree at '{}'",
                worktree_root
            );
            failed = true;
            continue;
        }
        // A symbolic-ref branch is deleted without a merge check (git resolves
        // it with RESOLVE_REF_NO_RECURSE); the symref itself is removed.
        if try_delete_symref_branch(store, &name, &branch, quiet)?.is_some() {
            continue;
        }
        let RefTarget::Direct(oid) = target else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        let Ok(tip) = sley_rev::peel_to_commit(db, format, &oid) else {
            eprintln!("error: branch '{branch}' not found");
            failed = true;
            continue;
        };
        let reachable = branch_delete_reachable_base(
            store,
            git_dir,
            db,
            format,
            &config,
            &name,
            head_reachable.as_ref(),
        )?;
        if !reachable.is_some_and(|reachable| reachable.contains(&tip)) {
            eprintln!("error: the branch '{branch}' is not fully merged");
            eprintln!("hint: If you are sure you want to delete it, run 'git branch -D {branch}'");
            eprintln!(
                "hint: Disable this message with \"git config set advice.forceDeleteBranch false\""
            );
            failed = true;
            continue;
        }
        let deleted_oid = delete_branch_ref(git_dir, store, &branch, &name)?;
        remove_branch_config(git_dir, &branch)?;
        if !quiet {
            let deleted_display = branch_delete_display(&branch, &name, &deleted_oid);
            println!("Deleted branch {branch} (was {deleted_display}).");
        }
    }

    if failed {
        return Err(GitError::Exit(1));
    }
    Ok(())
}

fn delete_branch_ref(
    git_dir: &Path,
    store: &FileRefStore,
    branch: &str,
    name: &str,
) -> Result<ObjectId> {
    if sley_worktree::worktree_root_for_git_dir(git_dir)?.is_none()
        && store.current_branch_ref()?.as_deref() == Some(name)
    {
        return Ok(store.delete_ref(name)?.oid);
    }
    Ok(store.delete_branch(branch)?.oid)
}

pub(super) fn branch_delete_display(branch: &str, refname: &str, oid: &ObjectId) -> String {
    if sley_refs::validate_ref_name(refname).is_err()
        && let Some((display, _)) = branch.split_once("...")
        && !display.is_empty()
    {
        return display.to_string();
    }
    short_oid(&oid.to_hex()).to_string()
}

pub(super) fn branch_delete_reachable_base<'a>(
    store: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    config: &GitConfig,
    refname: &str,
    head_reachable: Option<&'a HashSet<ObjectId>>,
) -> Result<Option<Cow<'a, HashSet<ObjectId>>>> {
    if let Some(upstream) = for_each_ref_upstream(config, refname)
        && let Some(target) = store.read_ref(&upstream.refname)?
    {
        let upstream_ref = sley_refs::Ref {
            name: upstream.refname,
            target,
        };
        if let Some((oid, _)) = resolve_for_each_ref_target(store, &upstream_ref)?
            && let Ok(commit) = sley_rev::peel_to_commit(db, format, &oid)
        {
            let reachable = sley_rev::reachable_commit_oids(git_dir, format, db, [commit], false)?;
            return Ok(Some(Cow::Owned(reachable)));
        }
    }
    Ok(head_reachable.map(Cow::Borrowed))
}
