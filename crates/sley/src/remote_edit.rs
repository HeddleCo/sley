//! Repository config load/save and `[remote]` editing.

use std::fs;

use sley_config::remotes::{self, RemoteEditError, SetUrlKind, SetUrlOp};
use sley_config::{ConfigEntry, ConfigSection, GitConfig};

use crate::{GitError, Repository, Result};

impl Repository {
    /// Read the repository config file (`<common_dir>/config`).
    ///
    /// Returns an empty [`GitConfig`] when the file is absent, matching
    /// [`Repository::config`].
    pub fn load_repo_config(&self) -> Result<GitConfig> {
        self.config()
    }

    /// Write `config` to `<common_dir>/config` using canonical serialization.
    ///
    /// Comments and original whitespace are not preserved (see
    /// [`GitConfig::to_canonical_bytes`]).
    pub fn save_repo_config(&self, config: &GitConfig) -> Result<()> {
        fs::write(self.common_dir().join("config"), config.to_canonical_bytes())?;
        Ok(())
    }

    /// Add `[remote "<name>"]` with `url` and the default fetch refspec.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        let mut config = self.load_repo_config()?;
        remotes::add_remote_with_fetch(&mut config, name, url, &[])
            .map_err(remote_edit_error)?;
        self.save_repo_config(&config)
    }

    /// Remove `[remote "<name>"]` and dependent branch configuration.
    pub fn remove_remote(&self, name: &str) -> Result<()> {
        let mut config = self.load_repo_config()?;
        remotes::remove_remote(&mut config, name).map_err(remote_edit_error)?;
        self.save_repo_config(&config)
    }

    /// Set the sole fetch URL for `[remote "<name>"]` (`git remote set-url`).
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<()> {
        let mut config = self.load_repo_config()?;
        remotes::set_url(
            &mut config,
            name,
            SetUrlKind::Fetch,
            SetUrlOp::Set { url },
        )
        .map_err(set_url_error)?;
        self.save_repo_config(&config)
    }

    /// Initialize a bare mirror repository at `path`.
    ///
    /// The resulting repository matches `git clone --mirror` defaults for a bare
    /// destination: `[remote "origin"]` carries `fetch = +refs/*:refs/*` and
    /// `mirror = true`. No `url` is written; callers typically follow with
    /// [`Repository::set_remote_url`] or [`Repository::add_remote`].
    pub fn init_mirror(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let repo = Repository::init_bare(path)?;
        let mut config = repo.load_repo_config()?;
        config.sections.push(ConfigSection {
            name: "remote".into(),
            subsection: Some("origin".into()),
            entries: vec![
                ConfigEntry {
                    key: "fetch".into(),
                    value: Some("+refs/*:refs/*".into()),
                    comment: None,
                },
                ConfigEntry {
                    key: "mirror".into(),
                    value: Some("true".into()),
                    comment: None,
                },
            ],
        });
        repo.save_repo_config(&config)?;
        Ok(repo)
    }
}

fn remote_edit_error(err: RemoteEditError) -> GitError {
    match err {
        RemoteEditError::AlreadyExists => {
            GitError::Command("remote already exists".into())
        }
        RemoteEditError::NotFound => GitError::NotFound("remote not found".into()),
    }
}

fn set_url_error(err: remotes::SetUrlError) -> GitError {
    match err {
        remotes::SetUrlError::RemoteNotFound => GitError::NotFound("remote not found".into()),
        remotes::SetUrlError::NoMatch => GitError::NotFound("remote url did not match".into()),
        remotes::SetUrlError::DeleteNoMatch => {
            GitError::NotFound("remote url did not match".into())
        }
        remotes::SetUrlError::DeleteAllFetchUrls => {
            GitError::Command("cannot delete every fetch url".into())
        }
        remotes::SetUrlError::MultipleValues => {
            GitError::Command("remote has multiple url values".into())
        }
    }
}