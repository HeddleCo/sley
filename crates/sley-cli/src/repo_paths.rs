use sley::{GitError, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    common_git_dir_for_git_dir_with_env(git_dir, true)
}

pub(crate) fn common_git_dir_for_git_dir_with_env(
    git_dir: &Path,
    honor_environment: bool,
) -> Result<PathBuf> {
    if honor_environment && let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        return Ok(PathBuf::from(common_dir));
    }
    let commondir = git_dir.join("commondir");
    if commondir.is_file() {
        let value = fs::read_to_string(&commondir)?;
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(common).map_err(|err| GitError::Io(err.to_string()));
    }
    fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()))
}

pub(crate) struct CommonGitDirSnapshot {
    pub path: PathBuf,
    pub linked_worktree: bool,
}

/// Resolve the common directory and retain the physical `commondir` marker
/// from the same probe. `GIT_COMMON_DIR` redirects common files but does not
/// erase linked-worktree identity.
pub(crate) fn common_git_dir_snapshot_with_env(
    git_dir: &Path,
    honor_environment: bool,
) -> Result<CommonGitDirSnapshot> {
    let commondir = git_dir.join("commondir");
    let linked_worktree = commondir.is_file();
    let path = if honor_environment && let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        PathBuf::from(common_dir)
    } else if linked_worktree {
        let value = fs::read_to_string(&commondir)?;
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        fs::canonicalize(common).map_err(|err| GitError::Io(err.to_string()))?
    } else {
        fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()))?
    };
    Ok(CommonGitDirSnapshot {
        path,
        linked_worktree,
    })
}
