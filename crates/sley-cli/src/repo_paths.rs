use sley::Result;
use std::path::{Path, PathBuf};

pub(crate) fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    common_git_dir_for_git_dir_with_env(git_dir, true)
}

pub(crate) fn common_git_dir_for_git_dir_with_env(
    git_dir: &Path,
    honor_environment: bool,
) -> Result<PathBuf> {
    sley::plumbing::sley_formats::repository_common_dir(git_dir, honor_environment)
}

pub(crate) struct CommonGitDirSnapshot {
    pub path: PathBuf,
    pub linked_worktree: bool,
}

/// Resolve the common directory via the canonical resolver and retain the
/// physical `commondir` marker from the same probe. `GIT_COMMON_DIR` redirects
/// common files but does not erase linked-worktree identity.
pub(crate) fn common_git_dir_snapshot_with_env(
    git_dir: &Path,
    honor_environment: bool,
) -> Result<CommonGitDirSnapshot> {
    let linked_worktree = git_dir.join("commondir").is_file();
    let path = sley::plumbing::sley_formats::repository_common_dir(git_dir, honor_environment)?;
    Ok(CommonGitDirSnapshot {
        path,
        linked_worktree,
    })
}
