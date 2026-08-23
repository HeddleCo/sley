//! Gitfile classification diagnostics and walk-up repository discovery probes.
//!
//! These are the discovery helpers that sit *below* the invocation setup engine:
//! the upward `.git` walk used to resolve a *remote* local-path repository
//! (without leaking the caller's `--git-dir` / `GIT_DIR` / `--bare` overrides
//! across the transport boundary), the single-filesystem discovery boundary, and
//! the same-directory comparison used when de-duplicating paths.
//!
//! The ceiling-directory, `device_of`, gitdir-candidate, and gitfile readers are
//! single-sourced from the parent [`super`] discovery module.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sley_core::{GitError, Result};

use super::ownership;
use super::{device_of, discovery_ceiling_directories, git_env_bool, is_git_dir};

/// Walk-up discovery only — no `--git-dir` / `GIT_DIR` / `--bare` overrides.
///
/// Used when resolving a *remote* local-path repository so local invocation
/// overrides do not leak across the transport boundary.
pub fn resolve_git_dir_walk_only(start: impl AsRef<Path>) -> Result<PathBuf> {
    resolve_git_dir_by_walk(start)
}

fn resolve_git_dir_by_walk(start: impl AsRef<Path>) -> Result<PathBuf> {
    let start = start.as_ref();
    let ceilings = discovery_ceiling_directories();
    let filesystem_boundary = discovery_filesystem_boundary(start);
    for candidate in start.ancestors() {
        if candidate != start
            && ceilings
                .iter()
                .any(|ceiling| ceiling.matches_discovery_candidate(candidate))
        {
            break;
        }
        let dot_git = candidate.join(".git");
        match probe_dot_git(&dot_git)? {
            DotGitProbe::Repo {
                git_dir,
                via_gitfile,
            } => {
                let gitfile = via_gitfile.then_some(dot_git.as_path());
                ownership::ensure_valid_ownership(Some(candidate), &git_dir, gitfile)?;
                return Ok(git_dir);
            }
            DotGitProbe::Continue => {}
        }
        if candidate.join("HEAD").is_file() && candidate.join("objects").is_dir() {
            ownership::note_implicit_bare_repository(candidate)?;
            ownership::ensure_valid_ownership(None, candidate, None)?;
            return Ok(candidate.to_path_buf());
        }
        if candidate.parent() == filesystem_boundary.as_deref() {
            break;
        }
    }
    Err(GitError::repository_not_found("not a git repository"))
}

/// The parent which upward discovery must not enter because it is on another
/// filesystem. `None` means discovery may walk to the filesystem root.
///
/// A configured ceiling takes precedence over a later filesystem boundary, as
/// it does in git's `setup_git_directory_gently_1`.
pub fn discovery_filesystem_boundary(start: &Path) -> Option<PathBuf> {
    if git_env_bool("GIT_DISCOVERY_ACROSS_FILESYSTEM") {
        return None;
    }

    let start_device = device_of(start)?;
    let ceilings = discovery_ceiling_directories();
    for candidate in start.ancestors() {
        if candidate != start
            && ceilings
                .iter()
                .any(|ceiling| ceiling.matches_discovery_candidate(candidate))
        {
            return None;
        }
        let parent = candidate.parent()?;
        if device_of(parent).is_some_and(|device| device != start_device) {
            return Some(parent.to_path_buf());
        }
    }
    None
}

enum DotGitProbe {
    Repo { git_dir: PathBuf, via_gitfile: bool },
    Continue,
}

fn probe_dot_git(dot_git: &Path) -> Result<DotGitProbe> {
    let metadata = match fs::metadata(dot_git) {
        Ok(metadata) => metadata,
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc_enotdir()) {
                return Ok(DotGitProbe::Continue);
            }
            return Err(invalid_gitfile_error(&format!(
                "error reading '{}'",
                dot_git.display()
            )));
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        if is_git_dir(dot_git) {
            return Ok(DotGitProbe::Repo {
                git_dir: dot_git.to_path_buf(),
                via_gitfile: false,
            });
        }
        return Ok(DotGitProbe::Continue);
    }
    if file_type.is_file() {
        return classify_gitfile(dot_git, metadata.len());
    }
    Err(invalid_gitfile_error(&format!(
        "not a regular file: '{}'",
        dot_git.display()
    )))
}

fn classify_gitfile(dot_git: &Path, size: u64) -> Result<DotGitProbe> {
    const MAX_GITFILE_SIZE: u64 = 1 << 20;
    if size > MAX_GITFILE_SIZE {
        return Err(invalid_gitfile_error(&format!(
            "too large to be a .git file: '{}'",
            dot_git.display()
        )));
    }
    let contents = match fs::read(dot_git) {
        Ok(contents) => contents,
        Err(_) => {
            return Err(invalid_gitfile_error(&format!(
                "error reading {}",
                dot_git.display()
            )));
        }
    };
    let Some(rest) = contents.strip_prefix(b"gitdir: ") else {
        return Err(invalid_gitfile_error(&format!(
            "invalid gitfile format: {}",
            dot_git.display()
        )));
    };
    let trimmed = trim_trailing_newlines(rest);
    if trimmed.is_empty() {
        return Err(invalid_gitfile_error(&format!(
            "no path in gitfile: {}",
            dot_git.display()
        )));
    }
    let raw_target = PathBuf::from(os_string_from_bytes(trimmed));
    let target = if raw_target.is_absolute() {
        raw_target
    } else {
        match dot_git.parent() {
            Some(parent) => parent.join(&raw_target),
            None => raw_target,
        }
    };
    if !is_git_dir(&target) {
        return Err(invalid_gitfile_error(&format!(
            "not a git repository: {}",
            target.display()
        )));
    }
    fs::canonicalize(&target)
        .map(|git_dir| DotGitProbe::Repo {
            git_dir,
            via_gitfile: true,
        })
        .map_err(|err| GitError::Io(err.to_string()))
}

fn invalid_gitfile_error(message: &str) -> GitError {
    GitError::InvalidFormat(format!("fatal: {message}"))
}

fn trim_trailing_newlines(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn libc_enotdir() -> i32 {
    20
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Whether two paths refer to the same directory, comparing lexically first and
/// then by canonicalized real path.
pub fn paths_refer_to_same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
