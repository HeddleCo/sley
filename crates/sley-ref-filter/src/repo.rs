//! Repository-backed lookups for for-each-ref: loose-object disk size and
//! worktree checkout paths.

use sley_core::{ObjectId, Result};
use sley_odb::repository_objects_dir;
use sley_worktree::worktree_root_for_git_dir;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn for_each_ref_loose_object_disk_size(
    git_dir: &Path,
    oid: &ObjectId,
) -> Result<Option<u64>> {
    let hex = oid.to_hex();
    if hex.len() < 2 {
        return Ok(None);
    }
    let (fanout, file) = hex.split_at(2);
    let path = repository_objects_dir(git_dir).join(fanout).join(file);
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn for_each_ref_worktree_path(
    git_dir: &Path,
    main_worktree_root: Option<&Path>,
    head_ref: Option<&str>,
    refname: &str,
) -> Result<Option<String>> {
    let main_worktree_root = main_worktree_root.map(PathBuf::from).or_else(|| {
        worktree_root_for_git_dir(git_dir).ok().flatten()
    });
    if head_ref == Some(refname)
        && let Some(worktree_root) = main_worktree_root
    {
        return Ok(Some(
            fs::canonicalize(worktree_root)?
                .to_string_lossy()
                .into_owned(),
        ));
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(None);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        if head.trim().strip_prefix("ref: ") != Some(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            return Ok(Some(
                fs::canonicalize(worktree_root)?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

/// Resolve every `refname -> checked-out worktree path` mapping in a single pass,
/// so `for-each-ref` need not re-scan `$GIT_DIR/worktrees` once per ref. Mirrors
/// the per-ref logic in [`for_each_ref_worktree_path`]: the current branch maps to
/// the main worktree root, and each linked worktree's `HEAD`/`gitdir` admin files
/// name the ref it has checked out and where its working tree lives.
pub fn for_each_ref_worktree_paths(
    git_dir: &Path,
    main_worktree_root: Option<&Path>,
    head_ref: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut paths = HashMap::new();
    let main_worktree_root = main_worktree_root.map(PathBuf::from).or_else(|| {
        worktree_root_for_git_dir(git_dir).ok().flatten()
    });
    if let Some(head_ref) = head_ref
        && let Some(worktree_root) = main_worktree_root
    {
        let canonical = fs::canonicalize(worktree_root)?;
        paths.insert(
            head_ref.to_string(),
            canonical.to_string_lossy().into_owned(),
        );
    }

    let worktrees_dir = git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(paths);
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        let Ok(head) = fs::read_to_string(admin_dir.join("HEAD")) else {
            continue;
        };
        let Some(refname) = head.trim().strip_prefix("ref: ") else {
            continue;
        };
        // The current branch's mapping (the main worktree root) takes precedence
        // and is already inserted above.
        if paths.contains_key(refname) {
            continue;
        }
        let Ok(gitdir) = fs::read_to_string(admin_dir.join("gitdir")) else {
            continue;
        };
        let gitdir = gitdir.trim();
        if gitdir.is_empty() {
            continue;
        }
        let gitdir_path = PathBuf::from(gitdir);
        let gitdir_path = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            admin_dir.join(gitdir_path)
        };
        if let Some(worktree_root) = gitdir_path.parent() {
            let canonical = fs::canonicalize(worktree_root)?;
            paths.insert(
                refname.to_string(),
                canonical.to_string_lossy().into_owned(),
            );
        }
    }
    Ok(paths)
}
