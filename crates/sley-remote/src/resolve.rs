//! Remote URL resolution and transport selection for embedders.
//!
//! Callers pass an effective [`GitConfig`] snapshot (see [`sley_config::load_effective_config`])
//! plus a remote name or literal URL; these helpers return rewritten URLs and the
//! corresponding [`FetchSource`] / [`PushDestination`] values expected by
//! [`crate::fetch`] and [`crate::push`].

use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_config::remotes::{resolve_remote_fetch_url, resolve_remote_push_url};
use sley_core::{GitError, Result};
use sley_odb::repository_common_dir;
use sley_transport::{RemoteTransport, RemoteUrl, parse_remote_url};

use crate::{FetchSource, PushDestination, RemoteTransportKind};

/// Resolve the fetch URL for `remote` using `config` (name lookup + `insteadOf`).
pub fn fetch_url(config: &GitConfig, remote: &str) -> String {
    resolve_remote_fetch_url(config, remote)
}

/// Resolve the push URL for `remote` using `config` (`pushurl` + `pushInsteadOf`).
pub fn push_url(config: &GitConfig, remote: &str) -> String {
    resolve_remote_push_url(config, remote)
}

/// Classify a rewritten URL for capability checks.
pub fn transport_kind_for_url(url: &str) -> Result<Option<RemoteTransportKind>> {
    if url.ends_with(".bundle") {
        return Ok(Some(RemoteTransportKind::Bundle));
    }
    Ok(match parse_remote_url(url)?.transport {
        RemoteTransport::Http | RemoteTransport::Https => Some(RemoteTransportKind::Http),
        RemoteTransport::Ssh | RemoteTransport::Ext => Some(RemoteTransportKind::Ssh),
        RemoteTransport::Git => Some(RemoteTransportKind::Git),
        RemoteTransport::Local | RemoteTransport::File => Some(RemoteTransportKind::Local),
    })
}

/// Build a [`FetchSource`] from a resolved URL.
///
/// `relative_base` is the directory relative paths are resolved against (typically
/// the repository working tree, or the parent of `.git` for a bare repo).
pub fn fetch_source_for_url(url: &str, relative_base: &Path) -> Result<FetchSource> {
    let parsed = parse_remote_url(url)?;
    source_from_parsed(&parsed, relative_base).map(FetchSource::from_concrete)
}

/// Build a [`PushDestination`] from a resolved URL.
pub fn push_destination_for_url(url: &str, relative_base: &Path) -> Result<PushDestination> {
    let parsed = parse_remote_url(url)?;
    source_from_parsed(&parsed, relative_base).map(PushDestination::from_concrete)
}

/// Resolve fetch URL rewriting and transport source in one step.
pub fn resolve_fetch_source(
    config: &GitConfig,
    remote: &str,
    relative_base: &Path,
) -> Result<(String, FetchSource)> {
    let url = fetch_url(config, remote);
    let source = fetch_source_for_url(&url, relative_base)?;
    Ok((url, source))
}

/// Resolve push URL rewriting and transport destination in one step.
pub fn resolve_push_destination(
    config: &GitConfig,
    remote: &str,
    relative_base: &Path,
) -> Result<(String, PushDestination)> {
    let url = push_url(config, remote);
    let destination = push_destination_for_url(&url, relative_base)?;
    Ok((url, destination))
}

enum ConcreteRemote {
    Network(RemoteUrl),
    Local {
        git_dir: PathBuf,
        common_git_dir: PathBuf,
    },
}

impl FetchSource {
    fn from_concrete(source: ConcreteRemote) -> Self {
        match source {
            ConcreteRemote::Network(remote) => match remote.transport {
                RemoteTransport::Http | RemoteTransport::Https => Self::Http(remote),
                RemoteTransport::Ssh | RemoteTransport::Ext => Self::Ssh(remote),
                RemoteTransport::Git => Self::Git {
                    remote,
                    protocol_v2: false,
                },
                RemoteTransport::Local | RemoteTransport::File => {
                    unreachable!("local remotes use FetchSource::Local")
                }
            },
            ConcreteRemote::Local {
                git_dir,
                common_git_dir,
            } => Self::Local {
                git_dir,
                common_git_dir,
            },
        }
    }
}

impl PushDestination {
    fn from_concrete(source: ConcreteRemote) -> Self {
        match source {
            ConcreteRemote::Network(remote) => match remote.transport {
                RemoteTransport::Http | RemoteTransport::Https => Self::Http(remote),
                RemoteTransport::Ssh | RemoteTransport::Ext => Self::Ssh(remote),
                RemoteTransport::Git => Self::Git(remote),
                RemoteTransport::Local | RemoteTransport::File => {
                    unreachable!("local remotes use PushDestination::Local")
                }
            },
            ConcreteRemote::Local {
                git_dir,
                common_git_dir,
            } => Self::Local {
                git_dir,
                common_git_dir,
            },
        }
    }
}

fn source_from_parsed(parsed: &RemoteUrl, relative_base: &Path) -> Result<ConcreteRemote> {
    match parsed.transport {
        RemoteTransport::Http
        | RemoteTransport::Https
        | RemoteTransport::Ssh
        | RemoteTransport::Ext
        | RemoteTransport::Git => Ok(ConcreteRemote::Network(parsed.clone())),
        RemoteTransport::Local | RemoteTransport::File => {
            let repo_path = local_repository_path(parsed, relative_base)?;
            let git_dir = discover_git_dir(&repo_path)?;
            Ok(ConcreteRemote::Local {
                common_git_dir: repository_common_dir(&git_dir),
                git_dir,
            })
        }
    }
}

fn local_repository_path(parsed: &RemoteUrl, relative_base: &Path) -> Result<PathBuf> {
    Ok(match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(&parsed.path);
            if path.is_absolute() {
                path
            } else {
                relative_base.join(path)
            }
        }
        RemoteTransport::File => PathBuf::from(&parsed.path),
        _ => {
            return Err(GitError::Unsupported("expected a local remote URL".into()));
        }
    })
}

/// Discover the git directory containing `start` (working tree or bare repo).
fn discover_git_dir(start: &Path) -> Result<PathBuf> {
    let absolute = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().map_err(GitError::from)?.join(start)
    };
    for candidate in absolute.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() {
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
    }
    Err(GitError::repository_not_found(format!(
        "not a git repository: {}",
        start.display()
    )))
}

fn is_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
        && (path.join("objects").is_dir() || path.join("commondir").is_file())
}

fn read_gitdir_link(path: &Path) -> Result<Option<PathBuf>> {
    let contents = std::fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    Ok(Some(if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or_else(|| Path::new("")).join(target)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_config::{ConfigEntry, ConfigSection, GitConfig};

    #[test]
    fn instead_of_rewrites_fetch_url() {
        let config = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![
                ConfigSection::new(
                    "remote",
                    Some("origin".into()),
                    vec![ConfigEntry::new(
                        "url",
                        Some("git@github.com:org/repo.git".into()),
                    )],
                ),
                ConfigSection::new(
                    "url",
                    Some("https://github.com/".into()),
                    vec![ConfigEntry::new(
                        "insteadOf",
                        Some("git@github.com:".into()),
                    )],
                ),
            ],
        };
        assert_eq!(
            fetch_url(&config, "origin"),
            "https://github.com/org/repo.git"
        );
    }

    #[test]
    fn push_url_prefers_pushurl() {
        let config = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![
                    ConfigEntry::new("url", Some("https://fetch.example/x.git".into())),
                    ConfigEntry::new("pushurl", Some("https://push.example/x.git".into())),
                ],
            )],
        };
        assert_eq!(push_url(&config, "origin"), "https://push.example/x.git");
    }

    #[test]
    fn git_scheme_routes_to_native_git_transport() {
        let url = "git://127.0.0.1/repo.git";

        assert_eq!(
            transport_kind_for_url(url).expect("kind"),
            Some(RemoteTransportKind::Git)
        );
        assert!(matches!(
            fetch_source_for_url(url, Path::new(".")).expect("fetch source"),
            FetchSource::Git { .. }
        ));
        assert!(matches!(
            push_destination_for_url(url, Path::new(".")).expect("push destination"),
            PushDestination::Git(_)
        ));
    }
}
