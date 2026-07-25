//! Remote URL resolution and transport selection for embedders.
//!
//! Callers pass an effective [`GitConfig`] snapshot (see [`sley_config::load_effective_config`])
//! plus a remote name or literal URL; these helpers return rewritten URLs and the
//! corresponding [`FetchSource`] / [`PushDestination`] values expected by
//! [`crate::fetch`] and [`crate::push`].

use std::path::{Path, PathBuf};

use sley_config::GitConfig;
use sley_config::remotes::{
    remote_config_values, remote_exists, resolve_remote_fetch_url, resolve_remote_push_url,
};
use sley_core::{GitError, Result};
use sley_odb::repository_common_dir;
use sley_transport::{RemoteTransport, RemoteUrl, parse_remote_url};

use crate::{FetchSource, PushDestination, RemoteTransportKind};

/// Explicit repository/process context for resolving a CLI remote without
/// consulting global cwd or repository-discovery state.
#[derive(Debug, Clone, Copy)]
pub struct RemoteResolutionContext<'a> {
    /// Invocation working directory used for literal relative paths.
    pub cwd: &'a Path,
    /// Current repository git directory, when the invocation has one.
    pub local_git_dir: Option<&'a Path>,
    /// Effective current-repository configuration, including injected values.
    pub config: Option<&'a GitConfig>,
}

/// A remote name/URL after config lookup and `insteadOf` rewriting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRemote {
    pub url: String,
    pub transport: RemoteTransport,
}

/// Resolve a remote name or literal URL using only explicit context.
pub fn resolve_remote(
    context: RemoteResolutionContext<'_>,
    repository: &str,
) -> Result<ResolvedRemote> {
    let url = context
        .config
        .map(|config| resolve_remote_fetch_url(config, repository))
        .unwrap_or_else(|| repository.to_string());
    let transport = parse_remote_url(&url)?.transport;
    Ok(ResolvedRemote { url, transport })
}

/// Resolve a remote name/path to a concrete local git directory.
///
/// This preserves Git's precedence: a configured remote name first, then the
/// literal path relative to the invocation cwd, then an `insteadOf` rewrite.
/// A `[remote "<name>"]` section without a `url` (for example only
/// `negotiationInclude`/`negotiationRestrict`) does not claim the name: fall
/// through to path-based resolution so `git push <path>` still works when
/// callers inject remote.* config for the path basename (t5516 negotiation).
pub fn resolve_local_remote_git_dir(
    context: RemoteResolutionContext<'_>,
    repository: &str,
) -> Result<PathBuf> {
    if let (Some(git_dir), Some(config)) = (context.local_git_dir, context.config)
        && remote_exists(config, repository)
        && !remote_config_values(config, repository, "url").is_empty()
    {
        return resolve_configured_local_remote_git_dir(config, repository, git_dir, context.cwd);
    }
    if let Ok(path) = local_repository_path_from_url(repository, context.cwd)
        && let Ok(git_dir) = discover_local_git_dir(&path)
    {
        return Ok(git_dir);
    }
    let local_git_dir = context
        .local_git_dir
        .ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
    let config = context
        .config
        .ok_or_else(|| GitError::repository_not_found("not a git repository"))?;
    let rewritten = resolve_remote_fetch_url(config, repository);
    if rewritten != repository
        && let Ok(path) = local_repository_path_from_url(&rewritten, context.cwd)
        && let Ok(git_dir) = discover_local_git_dir(&path)
    {
        return Ok(git_dir);
    }
    // Only claim a configured remote when one actually exists. A bare path that
    // is not a git repo (t5510 #55 `git fetch "a\!'b"`) must not surface as
    // "remote <name> url" not-found — git reports the path-shaped fatal instead.
    if remote_exists(config, repository)
        && !remote_config_values(config, repository, "url").is_empty()
    {
        return resolve_configured_local_remote_git_dir(
            config,
            repository,
            local_git_dir,
            context.cwd,
        );
    }
    Err(GitError::repository_not_found(repository.to_string()))
}

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

pub fn resolve_configured_local_remote_git_dir(
    config: &GitConfig,
    name: &str,
    git_dir: &Path,
    cwd: &Path,
) -> Result<PathBuf> {
    let url = remote_config_values(config, name, "url")
        .into_iter()
        .next()
        .ok_or_else(|| GitError::not_found(format!("remote {name} url")))?;
    let url = resolve_remote_fetch_url(config, &url);
    let parsed = parse_remote_url(&url)?;
    let remote_path = match parsed.transport {
        RemoteTransport::Local => {
            let path = PathBuf::from(parsed.path);
            if path.is_absolute() {
                path
            } else {
                repository_relative_path_base(git_dir, cwd)?.join(path)
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
    discover_local_git_dir(&remote_path)
}

fn repository_relative_path_base(git_dir: &Path, cwd: &Path) -> Result<PathBuf> {
    if git_dir.file_name().is_some_and(|name| name == ".git") {
        return git_dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| GitError::InvalidPath("git dir has no parent".into()));
    }
    Ok(cwd.to_path_buf())
}

fn local_repository_path_from_url(repository: &str, cwd: &Path) -> Result<PathBuf> {
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
            "local remote resolution requires a local repository".into(),
        )),
    }
}

pub fn discover_local_git_dir(path: &Path) -> Result<PathBuf> {
    let dot_git_path = path_with_dot_git_suffix(path);
    let candidates = [
        path.join(".git"),
        path.to_path_buf(),
        dot_git_path.join(".git"),
        dot_git_path,
    ];
    for candidate in candidates {
        if is_git_dir(&candidate) {
            return Ok(candidate);
        }
        if candidate.is_file()
            && let Some(git_dir) = read_gitdir_link(&candidate)?
            && is_git_dir(&git_dir)
        {
            return std::fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()));
        }
    }
    Err(GitError::repository_not_found("not a git repository"))
}

fn path_with_dot_git_suffix(path: &Path) -> PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(".git");
    PathBuf::from(suffixed)
}

fn percent_decode_url_path(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(GitError::InvalidPath(format!(
                    "invalid percent-encoded path {value:?}"
                )));
            }
            let high = percent_hex_value(bytes[index + 1]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            let low = percent_hex_value(bytes[index + 2]).ok_or_else(|| {
                GitError::InvalidPath(format!("invalid percent-encoded path {value:?}"))
            })?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
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

/// Discover the git directory containing `start` (working tree or bare repo).
fn discover_git_dir(start: &Path) -> Result<PathBuf> {
    for candidate in start.ancestors() {
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

    #[test]
    fn local_resolution_uses_only_injected_cwd_and_config() {
        let root = std::env::temp_dir().join(format!(
            "sley-remote-resolve-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let local = root.join("local");
        let git_dir = local.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n").expect("HEAD");
        let config = GitConfig {
            preamble: Vec::new(),
            suffix: Vec::new(),
            sections: vec![ConfigSection::new(
                "remote",
                Some("origin".into()),
                vec![ConfigEntry::new("url", Some("local".into()))],
            )],
        };
        let caller_git_dir = root.join(".git");
        let context = RemoteResolutionContext {
            cwd: &root,
            local_git_dir: Some(&caller_git_dir),
            config: Some(&config),
        };
        assert_eq!(
            resolve_local_remote_git_dir(context, "origin").expect("local remote"),
            git_dir
        );
        assert_eq!(
            resolve_remote(context, "origin")
                .expect("resolved remote")
                .url,
            "local"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
