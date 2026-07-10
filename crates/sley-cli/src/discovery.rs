//! Repository discovery for the CLI layer (walk-up, explicit `--git-dir`, bare mode).
//!
//! Invocation repository discovery is owned by [`crate::session::CliSession`].

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sley::{GitError, Result};

use crate::ownership;

pub(crate) fn resolve_cli_path(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn is_git_dir_candidate(path: &Path) -> bool {
    git_head_path_is_file_or_symlink(&path.join("HEAD"))
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn git_head_path_is_file_or_symlink(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file() || metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

pub(crate) fn read_gitdir_file(path: &Path) -> Result<Option<PathBuf>> {
    let contents = fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = target.trim();
    let target = PathBuf::from(target);
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        let parent = match path.parent() {
            Some(parent) => parent,
            None => Path::new(""),
        };
        Ok(Some(parent.join(target)))
    }
}

/// Walk-up discovery only — no `--git-dir` / `GIT_DIR` / `--bare` overrides.
///
/// Used when resolving a *remote* local-path repository so local invocation
/// overrides do not leak across the transport boundary.
pub(crate) fn resolve_git_dir_walk_only(start: impl AsRef<Path>) -> Result<PathBuf> {
    resolve_git_dir_by_walk(start)
}

fn resolve_git_dir_by_walk(start: impl AsRef<Path>) -> Result<PathBuf> {
    let ceilings = discovery_ceiling_directories();
    for candidate in start.as_ref().ancestors() {
        if candidate != start.as_ref()
            && ceilings
                .iter()
                .any(|ceiling| paths_refer_to_same_dir(ceiling, candidate))
        {
            break;
        }
        let dot_git = candidate.join(".git");
        match probe_dot_git(&dot_git)? {
            DotGitProbe::Repo {
                git_dir,
                via_gitfile,
            } => {
                let gitfile = via_gitfile.then(|| dot_git.as_path());
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
    }
    Err(GitError::repository_not_found("not a git repository"))
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
        if is_git_dir_candidate(dot_git) {
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
    if !is_git_dir_candidate(&target) {
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

fn discovery_ceiling_directories() -> Vec<PathBuf> {
    match env::var("GIT_CEILING_DIRECTORIES") {
        Ok(value) if !value.is_empty() => value
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn paths_refer_to_same_dir(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
