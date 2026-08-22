//! Process-environment overrides for repository discovery (`GIT_DIR`,
//! `GIT_WORK_TREE`, `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`).
//!
//! [`super::OpenOptions::respect_environment`] and [`super::Repository::open_from_environment`]
//! route through this module so embedders and harnesses get git-correct layout
//! resolution without a CLI front-end. The environment-discovery primitives
//! themselves live in [`sley_worktree::discovery`] so the hook engine
//! (`sley-hooks`) shares the exact same resolution rules.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sley_core::GitError;
use sley_worktree::discovery::{
    discover_git_dir_respecting_environment, environment_git_dir, environment_work_tree,
    is_git_dir, resolve_explicit_git_dir, resolve_path_from_cwd,
};

use super::{Result, resolve_git_dir};

/// Resolved repository layout when environment overrides apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvResolvedRepository {
    pub git_dir: PathBuf,
    pub work_tree_override: Option<PathBuf>,
}

/// Discover or open a repository honoring process environment variables.
pub(crate) fn resolve_repository(path: &Path, exact_path: bool) -> Result<EnvResolvedRepository> {
    let cwd = env::current_dir().map_err(|err| GitError::Io(err.to_string()))?;
    let relative_start = if path.as_os_str().is_empty() || path.is_absolute() {
        None
    } else {
        Some(cwd.join(path))
    };
    let start: &Path = if path.as_os_str().is_empty() {
        cwd.as_path()
    } else if path.is_absolute() {
        path
    } else {
        relative_start
            .as_ref()
            .expect("relative path prepared above")
    };

    let git_dir = if let Some(git_dir) = environment_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        resolve_explicit_git_dir(start, &git_dir)?
    } else if exact_path {
        resolve_git_dir(path)?
    } else {
        discover_git_dir_respecting_environment(start)?
    };

    if !is_git_dir(&git_dir) {
        return Err(GitError::repository_not_found(format!(
            "not a git repository: {}",
            git_dir.display()
        )));
    }

    let work_tree_override = environment_work_tree().map(|work_tree| {
        let resolved = resolve_path_from_cwd(&cwd, &work_tree);
        fs::canonicalize(&resolved).unwrap_or(resolved)
    });

    Ok(EnvResolvedRepository {
        git_dir,
        work_tree_override,
    })
}

// `open_from_environment_honors_git_dir_and_work_tree` lived here but required
// in-process `env::set_var`, which edition 2024 makes `unsafe` and the workspace
// forbids (`unsafe_code = "forbid"`). GIT_DIR / GIT_WORK_TREE override discovery
// is covered end-to-end instead: `crates/sley-cli/tests/{global_options,rev_parse}.rs`
// and upstream parity t1500-rev-parse / t1510-repo-setup / t2050-git-dir-relative.
