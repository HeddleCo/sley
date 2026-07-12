//! Explicit remote repository/session resolution for CLI frontends.

use super::config::read_repo_config;
use crate::*;
use std::path::{Path, PathBuf};

/// One snapshot of the invocation paths and effective repository config.
///
/// `ls-remote` may run outside a repository, so every repository field is
/// optional. `fetch` uses [`RemoteCommandContext::require_repository`].
#[derive(Debug, Clone)]
pub(crate) struct RemoteCommandContext {
    cwd: PathBuf,
    git_dir: Option<PathBuf>,
    repository: Option<sley::Repository>,
    config: Option<GitConfig>,
}

impl RemoteCommandContext {
    pub(crate) fn from_session(cli_session: &crate::session::CliSession) -> Self {
        let repository = cli_session.open_repository().ok();
        let config = repository
            .as_ref()
            .and_then(|repository| read_repo_config(repository.git_dir()).ok());
        Self {
            cwd: cli_session.cwd().to_path_buf(),
            git_dir: repository
                .as_ref()
                .map(|repository| repository.git_dir().to_path_buf()),
            repository,
            config,
        }
    }

    pub(crate) fn require_repository(cli_session: &crate::session::CliSession) -> Result<Self> {
        let repository = cli_session.open_repository()?;
        let config = read_repo_config(repository.git_dir())?;
        Ok(Self {
            cwd: cli_session.cwd().to_path_buf(),
            git_dir: Some(repository.git_dir().to_path_buf()),
            repository: Some(repository),
            config: Some(config),
        })
    }

    pub(crate) fn for_repository_paths(cwd: &Path, git_dir: &Path) -> Result<Self> {
        let repository = sley::Repository::open(git_dir)?;
        let config = read_repo_config(repository.git_dir())?;
        Ok(Self {
            cwd: cwd.to_path_buf(),
            git_dir: Some(repository.git_dir().to_path_buf()),
            repository: Some(repository),
            config: Some(config),
        })
    }

    pub(crate) fn from_explicit(cwd: &Path, git_dir: &Path, config: GitConfig) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            git_dir: Some(git_dir.to_path_buf()),
            repository: None,
            config: Some(config),
        }
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn git_dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }

    pub(crate) fn repository(&self) -> Option<&sley::Repository> {
        self.repository.as_ref()
    }

    pub(crate) fn config(&self) -> Option<&GitConfig> {
        self.config.as_ref()
    }

    pub(crate) fn required_git_dir(&self) -> Result<&Path> {
        self.git_dir()
            .ok_or_else(|| GitError::repository_not_found("not a git repository"))
    }

    pub(crate) fn required_repository(&self) -> Result<&sley::Repository> {
        self.repository()
            .ok_or_else(|| GitError::repository_not_found("not a git repository"))
    }

    pub(crate) fn required_config(&self) -> Result<&GitConfig> {
        self.config()
            .ok_or_else(|| GitError::repository_not_found("not a git repository"))
    }

    pub(crate) fn resolution(&self) -> sley_remote::RemoteResolutionContext<'_> {
        sley_remote::RemoteResolutionContext {
            cwd: self.cwd(),
            local_git_dir: self.git_dir(),
            config: self.config(),
        }
    }

    pub(crate) fn resolved_remote(&self, repository: &str) -> Result<sley_remote::ResolvedRemote> {
        sley_remote::resolve_remote(self.resolution(), repository)
    }
}

pub(crate) fn ls_remote_git_dir(
    context: &RemoteCommandContext,
    repository: &str,
) -> Result<PathBuf> {
    sley_remote::resolve_local_remote_git_dir(context.resolution(), repository)
}

pub(super) fn local_repository_git_dir_path(path: &Path) -> Result<PathBuf> {
    sley_remote::discover_local_git_dir(path)
}

pub(super) fn local_remote_git_dir(
    config: &GitConfig,
    name: &str,
    git_dir: &Path,
    cwd: &Path,
) -> Result<PathBuf> {
    sley_remote::resolve_configured_local_remote_git_dir(config, name, git_dir, cwd)
}
