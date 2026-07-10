//! Branch rename and copy.

use super::config::{copy_branch_config, rename_branch_config};
use super::delete::branch_checked_out_worktree_path;
use super::operand::{
    BranchOperandKind, branch_resolve_local_branch_operand, validate_branch_creation_name,
};
use crate::*;

#[derive(Clone, Copy)]
pub(super) enum BranchMoveKind {
    Rename,
    Copy,
}

pub(super) struct BranchMoveOptions {
    pub(crate) kind: BranchMoveKind,
    pub(crate) force: bool,
    pub(crate) branches: Vec<String>,
}

#[rustfmt::skip]
pub(super) fn run_branch_move_options(
    git_dir: &Path,
    store: &FileRefStore,
    options: BranchMoveOptions,
) -> Result<()> {
    let (old_branch, new_branch) = branch_move_branches(store, options.kind, &options.branches)?;
    let format = repository_object_format(git_dir)?;
    let (old_branch, old_ref) = branch_resolve_local_branch_operand(
        git_dir,
        format,
        store,
        &old_branch,
        BranchOperandKind::Existing,
    )?;
    if old_branch == new_branch {
        return Ok(());
    }
    let new_ref = validate_branch_creation_name(&new_branch)?;
    if store.read_ref(&old_ref)?.is_none() {
        // branch.c `copy_or_rename_branch`: renaming the current *unborn*
        // branch (HEAD points at it but no commit exists) is allowed and only
        // repoints the HEAD symref; copying it (or touching any other missing
        // branch) dies.
        let old_is_head = store.current_branch_ref()?.as_deref() == Some(old_ref.as_str());
        if matches!(options.kind, BranchMoveKind::Rename)
            && (old_is_head || any_worktree_head_points_at(git_dir, &old_ref)?)
        {
            if !options.force && store.read_ref(&new_ref)?.is_some() {
                eprintln!("fatal: a branch named '{new_branch}' already exists");
                return Err(GitError::Exit(128));
            }
            if old_is_head {
                let mut tx = store.transaction();
                tx.update(RefUpdate {
                    name: "HEAD".into(),
                    expected: None,
                    new: RefTarget::Symbolic(new_ref.clone()),
                    reflog: None,
                });
                tx.commit()?;
            }
            update_all_worktree_heads(git_dir, &old_ref, &new_ref)?;
            rename_branch_config(git_dir, &old_branch, &new_branch)?;
            return Ok(());
        }
        if old_is_head {
            eprintln!("fatal: no commit on branch '{old_branch}' yet");
        } else {
            eprintln!("fatal: no branch named '{old_branch}'");
        }
        return Err(GitError::Exit(128));
    }
    // A dangling symref destination does not "exist" for the purposes of the
    // rename collision check (git's validate_branchname uses RESOLVE_REF_READING),
    // so `branch -m m broken_symref` overwrites it without --force (t3200 #16).
    if matches!(options.kind, BranchMoveKind::Rename)
        && !any_worktree_head_points_at(git_dir, &old_ref)?
        && let Some(worktree) = sley_worktree::find_shared_symref(git_dir, "HEAD", &old_ref)?
    {
        let operation = if worktree
            .path
            .join(".git")
            .is_file()
            && worktree_has_bisect_start(&worktree.path)
        {
            "bisected"
        } else {
            "rebased"
        };
        eprintln!(
            "fatal: branch {old_ref} is being {operation} at {}",
            worktree.path.display()
        );
        return Err(GitError::Exit(128));
    }
    if !options.force && sley_refs::resolve_ref_peeled(store, &new_ref)?.is_some() {
        eprintln!("fatal: a branch named '{new_branch}' already exists");
        return Err(GitError::Exit(128));
    }
    if options.force
        && old_ref != new_ref
        && store.read_ref(&new_ref)?.is_some()
        && let Some(worktree_root) = branch_checked_out_worktree_path(git_dir, store, &new_ref)?
    {
        eprintln!(
            "fatal: cannot force update the branch '{new_branch}' used by worktree at '{}'",
            worktree_root
        );
        return Err(GitError::Exit(128));
    }

    match options.kind {
        BranchMoveKind::Rename => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            let head_was_old = store.current_branch_ref()?.as_deref() == Some(old_ref.as_str());
            let old_oid = match store.read_ref(&old_ref)? {
                Some(RefTarget::Direct(oid)) => oid,
                _ => zero_oid(repository_object_format(git_dir)?)?,
            };
            let head_reflog_message =
                format!("Branch: renamed {old_ref} to {new_ref}").into_bytes();
            if let Err(err) = sley_refs::branch::transfer_branch(
                store,
                sley_refs::branch::BranchTransferOptions {
                    kind: sley_refs::branch::BranchTransferKind::Move,
                    source: old_branch.clone(),
                    destination: new_branch.clone(),
                    force: options.force,
                    committer,
                },
            ) {
                let err = err.into_git_error();
                return branch_move_failed(err, "rename");
            }
            let linked_update = update_all_worktree_heads(git_dir, &old_ref, &new_ref);
            if head_was_old {
                let null_oid = ObjectId::null(repository_object_format(git_dir)?);
                let committer = branch_reflog_committer_identity(store, &new_branch)?;
                store.append_reflog(
                    "HEAD",
                    &ReflogEntry {
                        old_oid,
                        new_oid: null_oid,
                        committer: committer.clone(),
                        message: head_reflog_message.clone(),
                    },
                )?;
                store.append_reflog(
                    "HEAD",
                    &ReflogEntry {
                        old_oid: null_oid,
                        new_oid: old_oid,
                        committer,
                        message: head_reflog_message,
                    },
                )?;
            }
            rename_branch_config(git_dir, &old_branch, &new_branch)?;
            linked_update?;
        }
        BranchMoveKind::Copy => {
            let committer = branch_reflog_committer_identity(store, &old_branch)?;
            if let Err(err) = sley_refs::branch::transfer_branch(
                store,
                sley_refs::branch::BranchTransferOptions {
                    kind: sley_refs::branch::BranchTransferKind::Copy,
                    source: old_branch.clone(),
                    destination: new_branch.clone(),
                    force: options.force,
                    committer,
                },
            ) {
                let err = err.into_git_error();
                return branch_move_failed(err, "copy");
            }
            copy_branch_config(git_dir, &old_branch, &new_branch)?;
        }
    }
    Ok(())
}

fn worktree_has_bisect_start(worktree_path: &Path) -> bool {
    let dotgit = worktree_path.join(".git");
    let Ok(contents) = fs::read_to_string(dotgit) else {
        return false;
    };
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let target = PathBuf::from(target.trim());
    let admin_dir = if target.is_absolute() {
        target
    } else {
        worktree_path.join(target)
    };
    admin_dir.join("BISECT_START").is_file()
}

pub(super) fn branch_move_failed(err: GitError, operation: &str) -> Result<()> {
    match err {
        GitError::Transaction(message) => {
            eprintln!("error: {message}");
            eprintln!("fatal: branch {operation} failed");
            Err(GitError::Exit(128))
        }
        err => Err(err),
    }
}

pub(super) fn any_worktree_head_points_at(git_dir: &Path, refname: &str) -> Result<bool> {
    Ok(!worktree_head_paths(git_dir, refname)?.is_empty())
}

pub(super) fn update_all_worktree_heads(
    git_dir: &Path,
    old_ref: &str,
    new_ref: &str,
) -> Result<()> {
    let mut failed = false;
    for head_path in worktree_head_paths(git_dir, old_ref)? {
        if head_path.with_file_name("HEAD.lock").exists() {
            failed = true;
            continue;
        }
        fs::write(head_path, format!("ref: {new_ref}\n"))?;
    }
    if failed {
        eprintln!("error: could not update one or more linked worktree HEADs");
        return Err(GitError::Exit(1));
    }
    Ok(())
}

pub(super) fn worktree_head_paths(git_dir: &Path, refname: &str) -> Result<Vec<PathBuf>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let mut heads = Vec::new();
    let main_head = common_git_dir.join("HEAD");
    if fs::read_to_string(&main_head)
        .ok()
        .and_then(|head| head.trim().strip_prefix("ref: ").map(str::to_string))
        .as_deref()
        == Some(refname)
    {
        heads.push(main_head);
    }

    let worktrees_dir = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(heads);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let head_path = admin_dir.join("HEAD");
        let Ok(head) = fs::read_to_string(&head_path) else {
            continue;
        };
        if head.trim().strip_prefix("ref: ") != Some(refname) {
            continue;
        }
        heads.push(head_path);
    }
    Ok(heads)
}

pub(super) fn branch_reflog_committer_identity(
    store: &FileRefStore,
    branch: &str,
) -> Result<Vec<u8>> {
    if env::var("GIT_COMMITTER_DATE").is_ok() {
        return commit_identity_from_env("COMMITTER");
    }
    let refname = branch_ref_name(branch)?;
    let max_existing = store
        .read_reflog(&refname)?
        .iter()
        .filter_map(|entry| entry.timestamp_seconds().ok())
        .max()
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let date = format!("@{} +0000", now.max(max_existing + 1));
    let name = env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| "Git Rs".into());
    let email = env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| "sley@example.invalid".into());
    sley_sequencer::format_commit_identity(&name, &email, &date)
}

pub(super) fn branch_move_branches(
    store: &FileRefStore,
    kind: BranchMoveKind,
    branches: &[String],
) -> Result<(String, String)> {
    match branches {
        [] => {
            eprintln!("fatal: branch name required");
            Err(GitError::Exit(128))
        }
        [new_branch] => {
            let Some(old_branch) = store.current_branch()? else {
                match kind {
                    BranchMoveKind::Rename => {
                        eprintln!("fatal: cannot rename the current branch while not on any");
                    }
                    BranchMoveKind::Copy => {
                        eprintln!("fatal: cannot copy the current branch while not on any");
                    }
                }
                return Err(GitError::Exit(128));
            };
            Ok((old_branch, new_branch.to_string()))
        }
        [old_branch, new_branch] => Ok((old_branch.to_string(), new_branch.to_string())),
        _ => {
            match kind {
                BranchMoveKind::Rename => {
                    eprintln!("fatal: too many arguments for a rename operation");
                }
                BranchMoveKind::Copy => {
                    eprintln!("fatal: too many branches for a copy operation");
                }
            }
            Err(GitError::Exit(128))
        }
    }
}
