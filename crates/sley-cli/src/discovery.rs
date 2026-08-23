//! CLI shims over the consolidated repository-discovery engine.
//!
//! The discovery engine (walk-up probing, gitfile classification diagnostics,
//! ceiling/filesystem boundaries, `safe.directory` ownership checks) lives in
//! [`sley_worktree::discovery`]. This module keeps the CLI-facing aliases and
//! the one CLI-specific path helper.

use std::path::{Path, PathBuf};

pub(crate) use crate::sley_worktree::discovery::probes::discovery_filesystem_boundary;
pub(crate) use crate::sley_worktree::discovery::probes::paths_refer_to_same_dir;
pub(crate) use crate::sley_worktree::discovery::probes::resolve_git_dir_walk_only;
pub(crate) use crate::sley_worktree::discovery::{is_git_dir as is_git_dir_candidate, read_gitdir_link as read_gitdir_file};

/// Resolve a user-provided textual path against the invocation cwd when it is
/// not already absolute.
pub(crate) fn resolve_cli_path(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
