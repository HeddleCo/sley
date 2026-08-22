//! Process-environment repository discovery (`GIT_DIR`, `GIT_WORK_TREE`,
//! `GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`).
//!
//! Shared by the embeddable facade's `open_env` setup path and the hook engine
//! (`sley-hooks`), which must resolve a git directory from the current working
//! directory with git's environment overrides when callers do not supply one.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sley_core::{GitError, Result};

/// `GIT_DIR` when set in the process environment.
pub fn environment_git_dir() -> Option<PathBuf> {
    env::var_os("GIT_DIR").map(PathBuf::from)
}

/// `GIT_WORK_TREE` when set in the process environment.
pub fn environment_work_tree() -> Option<PathBuf> {
    env::var_os("GIT_WORK_TREE").map(PathBuf::from)
}

/// Resolve `value` relative to `cwd` when it is not absolute.
pub fn resolve_path_from_cwd(cwd: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        cwd.join(value)
    }
}

/// Walk upward from `start`, honoring `GIT_DIR` when set and discovery ceilings.
pub fn discover_git_dir_respecting_environment(start: &Path) -> Result<PathBuf> {
    if let Some(git_dir) = environment_git_dir() {
        if git_dir.as_os_str().is_empty() {
            return Err(GitError::repository_not_found("not a git repository"));
        }
        return resolve_explicit_git_dir(start, &git_dir);
    }
    discover_git_dir_with_ceilings(start)
}

/// Resolve an explicitly provided `GIT_DIR` (already known to be non-empty)
/// against `start`, following a `gitdir:` gitlink file when applicable.
pub fn resolve_explicit_git_dir(start: &Path, git_dir: &Path) -> Result<PathBuf> {
    let resolved = resolve_path_from_cwd(start, git_dir);
    if resolved.is_file()
        && let Some(target) = read_gitdir_link(&resolved)?
        && is_git_dir(&target)
    {
        return fs::canonicalize(target).map_err(|err| GitError::Io(err.to_string()));
    }
    Ok(resolved)
}

fn discover_git_dir_with_ceilings(start: &Path) -> Result<PathBuf> {
    let start = if start.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start
    };
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| GitError::Io(err.to_string()))?
            .join(start)
    };

    let one_filesystem = !git_env_bool("GIT_DISCOVERY_ACROSS_FILESYSTEM");
    let start_device = if one_filesystem {
        device_of(&absolute)
    } else {
        None
    };
    let ceilings = discovery_ceiling_directories();

    for candidate in absolute.ancestors() {
        if candidate != absolute.as_path()
            && ceilings
                .iter()
                .any(|ceiling| ceiling.matches_discovery_candidate(candidate))
        {
            break;
        }

        let dot_git = candidate.join(".git");
        if dot_git.is_dir() && is_git_dir(&dot_git) {
            return Ok(dot_git);
        }
        if dot_git.is_file()
            && let Some(git_dir) = read_gitdir_link(&dot_git)?
            && is_git_dir(&git_dir)
        {
            return Ok(git_dir);
        }
        if is_git_dir(candidate) {
            return Ok(candidate.to_path_buf());
        }

        if one_filesystem
            && let Some(parent) = candidate.parent()
            && device_of(parent) != start_device
        {
            break;
        }
    }

    Err(GitError::repository_not_found(format!(
        "not a git repository (or any parent up to {}): {}",
        absolute.display(),
        start.display()
    )))
}

/// One `GIT_CEILING_DIRECTORIES` entry after git's `canonicalize_ceiling_entry`
/// processing (empty entries disable realpath for subsequent ceilings).
struct CeilingDirectory {
    path: PathBuf,
    resolved: bool,
}

impl CeilingDirectory {
    fn matches_discovery_candidate(&self, candidate: &Path) -> bool {
        let ceiling = strip_trailing_slashes(&self.path);
        let candidate_raw = strip_trailing_slashes(candidate);
        if ceiling.as_os_str() == candidate_raw.as_os_str() {
            return true;
        }
        if !self.resolved {
            return false;
        }
        match fs::canonicalize(candidate) {
            Ok(canonical) => strip_trailing_slashes(&canonical).as_os_str() == ceiling.as_os_str(),
            Err(_) => false,
        }
    }
}

fn discovery_ceiling_directories() -> Vec<CeilingDirectory> {
    let Ok(value) = env::var("GIT_CEILING_DIRECTORIES") else {
        return Vec::new();
    };
    if value.is_empty() {
        return Vec::new();
    }
    let mut empty_entry_found = false;
    let mut out = Vec::new();
    for entry in value.split(':') {
        if entry.is_empty() {
            empty_entry_found = true;
            continue;
        }
        let path = Path::new(entry);
        if !path.is_absolute() {
            continue;
        }
        if empty_entry_found {
            out.push(CeilingDirectory {
                path: strip_trailing_slashes(path),
                resolved: false,
            });
            continue;
        }
        if let Ok(canonical) = fs::canonicalize(path) {
            out.push(CeilingDirectory {
                path: strip_trailing_slashes(&canonical),
                resolved: true,
            });
        }
    }
    out
}

fn strip_trailing_slashes(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        PathBuf::from(trimmed)
    }
}

fn git_env_bool(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|metadata| metadata.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// True if `path` looks like a git directory (has a `HEAD` file and either an
/// `objects` directory or a `commondir` pointer).
pub fn is_git_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join("HEAD"))
        .is_ok_and(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

/// Read a `gitdir: <path>` link file (used by linked worktrees and submodules),
/// returning the absolute target path it points at.
pub fn read_gitdir_link(path: &Path) -> Result<Option<PathBuf>> {
    let contents = fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        Ok(Some(base.join(target)))
    }
}
