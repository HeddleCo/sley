//! Resolve a repository name/URL to a local git directory.

use super::config::{read_repo_config, remote_exists};
use crate::remote::{remote_config_values, rewrite_url_with_config};
use crate::*;
use std::path::{Path, PathBuf};

pub(crate) fn ls_remote_git_dir(repository: &str) -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    let local_git_dir = crate::session::cli_git_dir_from(&cwd).ok();
    if let Some(git_dir) = local_git_dir.as_deref() {
        let config = read_repo_config(git_dir)?;
        if remote_exists(&config, repository) {
            return local_remote_git_dir(&config, repository, git_dir);
        }
    }
    if let Ok(path) = ls_remote_repository_path(repository, &cwd)
        && let Ok(git_dir) = local_repository_git_dir_path(&path)
    {
        return Ok(git_dir);
    }
    let local_git_dir =
        local_git_dir.ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
    let config = read_repo_config(&local_git_dir)?;
    let rewritten = rewrite_url_with_config(&config, repository, false);
    if rewritten != repository
        && let Ok(path) = ls_remote_repository_path(&rewritten, &cwd)
        && let Ok(git_dir) = local_repository_git_dir_path(&path)
    {
        return Ok(git_dir);
    }
    local_remote_git_dir(&config, repository, &local_git_dir)
}

pub(super) fn local_repository_git_dir_path(path: &Path) -> Result<PathBuf> {
    let dot_git_path = path_with_dot_git_suffix(path);
    let candidates = [
        path.join(".git"),
        path.to_path_buf(),
        dot_git_path.join(".git"),
        dot_git_path,
    ];
    for candidate in candidates {
        if remote_git_dir_candidate(&candidate) {
            return Ok(candidate);
        }
        if candidate.is_file()
            && let Some(git_dir) = read_gitdir_file(&candidate)?
            && remote_git_dir_candidate(&git_dir)
        {
            return fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()));
        }
    }
    Err(GitError::repository_not_found("not a git repository"))
}

fn path_with_dot_git_suffix(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(".git");
    PathBuf::from(suffixed)
}

fn remote_git_dir_candidate(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn ls_remote_repository_path(repository: &str, cwd: &Path) -> Result<PathBuf> {
    let parsed = parse_remote_url(repository)?;
    match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(parsed.path);
            Ok(if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            })
        }
        RemoteTransport::File => Ok(PathBuf::from(percent_decode_url_path(&parsed.path)?)),
        RemoteTransport::Ssh
        | RemoteTransport::Ext
        | RemoteTransport::Git
        | RemoteTransport::Http
        | RemoteTransport::Https => Err(GitError::Unsupported(
            "ls-remote currently supports local repositories".into(),
        )),
    }
}

pub(super) fn percent_decode_url_path(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(GitError::InvalidPath(format!(
                    "invalid percent-encoded path {value:?}"
                )));
            }
            let high = percent_hex_value(bytes[i + 1]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            let low = percent_hex_value(bytes[i + 2]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            decoded.push((high << 4) | low);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| GitError::InvalidPath(format!("invalid utf-8 file URL path {value:?}")))
}

fn percent_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
pub(super) fn local_remote_git_dir(config: &GitConfig, name: &str, git_dir: &Path) -> Result<PathBuf> {
    let url = remote_config_values(config, name, "url")
        .into_iter()
        .next()
        .ok_or_else(|| GitError::not_found(format!("remote {name} url")))?;
    let url = rewrite_url_with_config(config, &url, false);
    let parsed = parse_remote_url(&url)?;
    let remote_path = match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(parsed.path);
            if path.is_absolute() {
                path
            } else {
                repository_relative_path_base(git_dir)?.join(path)
            }
        }
        RemoteTransport::File => PathBuf::from(percent_decode_url_path(&parsed.path)?),
        RemoteTransport::Ssh
        | RemoteTransport::Ext
        | RemoteTransport::Git
        | RemoteTransport::Http
        | RemoteTransport::Https => {
            return Err(GitError::Unsupported(
                "remote discovery for non-local transports".into(),
            ));
        }
    };
    local_repository_git_dir_path(&remote_path)
}

fn repository_relative_path_base(git_dir: &Path) -> Result<PathBuf> {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| GitError::InvalidPath("git dir has no parent".into()));
    }
    env::current_dir().map_err(GitError::from)
}
