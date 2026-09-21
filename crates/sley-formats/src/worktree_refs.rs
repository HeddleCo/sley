//! Read-only worktree metadata used to protect refs during fetch and branch updates.

use sley_core::{Result, paths::normalize_lexical};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::worktree_root_for_git_dir;

/// Delegates to the canonical resolver in [`crate::repository_common_dir`] (environment always
/// honored, errors propagate).
pub fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    crate::repository_common_dir(git_dir, true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSymrefWorktree {
    pub refname: String,
    pub path: PathBuf,
}

struct WorktreeAdmin {
    git_dir: PathBuf,
    path: Option<PathBuf>,
}

/// If `refname` is listed in any in-progress rebase's `update-refs` file,
/// return the worktree that owns that rebase (mirrors git's treatment of those
/// refs as checked-out for `branch -f` / `worktree add` guards).
pub fn worktree_holding_rebase_update_ref(
    git_dir: &Path,
    refname: &str,
) -> Result<Option<SharedSymrefWorktree>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for admin in worktree_admins(&common_git_dir)? {
        let Some(path) = admin.path.clone() else {
            continue;
        };
        if worktree_rebase_update_refs(&admin.git_dir)
            .iter()
            .any(|name| name == refname)
        {
            return Ok(Some(SharedSymrefWorktree {
                refname: refname.to_string(),
                path,
            }));
        }
    }
    Ok(None)
}

pub fn find_shared_symref(
    git_dir: &Path,
    symref: &str,
    target: &str,
) -> Result<Option<SharedSymrefWorktree>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for admin in worktree_admins(&common_git_dir)? {
        // git's `is_shared_symref` returns 0 for a bare worktree: a bare main
        // repo has no working tree, so its `HEAD` branch is never "checked out"
        // and must not block updates (t5516 "… bare repository worktree").
        let Some(path) = admin.path.clone() else {
            continue;
        };
        if worktree_uses_symref(&admin.git_dir, symref, target)? {
            return Ok(Some(SharedSymrefWorktree {
                refname: target.to_string(),
                path,
            }));
        }
    }
    Ok(None)
}

pub fn worktree_refs_in_use(git_dir: &Path) -> Result<HashSet<String>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let mut refs = HashSet::new();
    for admin in worktree_admins(&common_git_dir)? {
        if let Ok(head) = fs::read_to_string(admin.git_dir.join("HEAD")) {
            let head = head.trim();
            if let Some(target) = head.strip_prefix("ref: ") {
                refs.insert(target.to_string());
            }
            refs.extend(worktree_detached_operation_refs(&admin.git_dir));
        }
    }
    Ok(refs)
}

fn worktree_admins(common_git_dir: &Path) -> Result<Vec<WorktreeAdmin>> {
    let mut admins = Vec::new();
    admins.push(WorktreeAdmin {
        git_dir: common_git_dir.to_path_buf(),
        path: worktree_root_for_git_dir(common_git_dir)?,
    });
    let worktrees_dir = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(admins);
    };
    for entry in entries {
        let entry = entry?;
        let git_dir = entry.path();
        let path = linked_worktree_path(&git_dir);
        admins.push(WorktreeAdmin { git_dir, path });
    }
    Ok(admins)
}

fn linked_worktree_path(admin_dir: &Path) -> Option<PathBuf> {
    let gitdir = fs::read_to_string(admin_dir.join("gitdir")).ok()?;
    let gitdir = gitdir.trim();
    if gitdir.is_empty() {
        return None;
    }
    let gitdir_path = resolve_worktree_admin_path(admin_dir, gitdir);
    gitdir_path
        .parent()
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical(path)))
}

fn worktree_uses_symref(git_dir: &Path, symref: &str, target: &str) -> Result<bool> {
    if symref != "HEAD" {
        return Ok(false);
    }
    if worktree_head_symref_target(&git_dir.join(symref)).as_deref() == Some(target) {
        return Ok(true);
    }
    if worktree_rebase_update_refs(git_dir)
        .iter()
        .any(|name| name == target)
    {
        return Ok(true);
    }
    if worktree_detached_operation_uses_ref(git_dir, target) {
        return Ok(true);
    }
    Ok(false)
}

fn worktree_head_symref_target(path: &Path) -> Option<String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
        && let Ok(target) = fs::read_link(path)
    {
        let target = target.to_string_lossy();
        if target.starts_with("refs/") {
            return Some(target.into_owned());
        }
    }
    let head = fs::read_to_string(path).ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_string)
}

fn worktree_detached_operation_uses_ref(git_dir: &Path, target: &str) -> bool {
    worktree_detached_operation_refs(git_dir)
        .iter()
        .any(|name| name == target)
}

fn worktree_detached_operation_refs(git_dir: &Path) -> Vec<String> {
    let mut refs = Vec::new();
    for dir in ["rebase-merge", "rebase-apply"] {
        let Some(refname) = operation_head_name_ref(git_dir.join(dir).join("head-name")) else {
            continue;
        };
        refs.push(refname);
    }
    refs.extend(worktree_rebase_update_refs(git_dir));
    if let Some(refname) = operation_head_name_ref(git_dir.join("BISECT_START")) {
        refs.push(refname);
    }
    refs
}

fn worktree_rebase_update_refs(git_dir: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(git_dir.join("rebase-merge").join("update-refs")) else {
        return Vec::new();
    };
    text.lines()
        .step_by(3)
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| line.to_string())
        })
        .collect()
}

fn operation_head_name_ref(path: PathBuf) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("refs/heads/") {
        Some(value.to_string())
    } else {
        Some(format!("refs/heads/{value}"))
    }
}

/// Resolve a path read from a git-directory administrative file (e.g. the
/// `gitdir` link of a linked worktree): absolute paths are kept as-is, relative
/// paths are joined onto the administrative directory.
fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepositoryLayout;
    use sley_core::ObjectFormat;

    #[test]
    fn bare_head_is_not_checked_out_but_linked_worktree_and_rebase_refs_are() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout =
            RepositoryLayout::init_at(temp.path().join("repo.git"), ObjectFormat::Sha1, true)
                .expect("bare repository");
        let git_dir = &layout.git_dir;
        assert!(
            find_shared_symref(git_dir, "HEAD", "refs/heads/main")
                .expect("bare HEAD")
                .is_none()
        );

        let worktree = temp.path().join("linked");
        fs::create_dir(&worktree).expect("worktree");
        let admin = git_dir.join("worktrees/linked");
        fs::create_dir_all(admin.join("rebase-merge")).expect("admin");
        fs::write(
            admin.join("gitdir"),
            worktree.join(".git").to_string_lossy().as_bytes(),
        )
        .expect("gitdir link");
        fs::write(admin.join("commondir"), "../..").expect("common dir");
        fs::write(admin.join("HEAD"), "ref: refs/heads/topic\n").expect("HEAD");
        fs::write(
            admin.join("rebase-merge/head-name"),
            "refs/heads/rebasing\n",
        )
        .expect("rebase HEAD");
        fs::write(
            admin.join("rebase-merge/update-refs"),
            "refs/heads/updated\nold\nnew\n",
        )
        .expect("rebase refs");
        for name in ["topic", "rebasing", "updated"] {
            let target = format!("refs/heads/{name}");
            let found = find_shared_symref(git_dir, "HEAD", &target)
                .expect("protection")
                .expect("protected ref");
            assert_eq!(found.refname, target);
            assert_eq!(found.path, worktree.canonicalize().expect("path"));
        }
        assert!(
            find_shared_symref(git_dir, "HEAD", "refs/heads/unused")
                .expect("unused ref")
                .is_none()
        );
    }
}
