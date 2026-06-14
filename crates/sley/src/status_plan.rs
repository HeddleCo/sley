//! Reusable status/index work plans for embedders.

use crate::{
    GitError, Repository, Result, ShortStatusEntry, ShortStatusOptions, StatusUntrackedMode,
};

/// Caller-owned key for status/index cache reuse.
///
/// The current facade records the key so callers can keep one plan identity
/// across commands; deeper shared-cache storage can attach behind this type
/// without changing Heddle call sites.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatusCacheKey(String);

impl StatusCacheKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for StatusCacheKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StatusCacheKey {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Builder for a repository status work plan.
#[derive(Debug, Clone)]
pub struct StatusPlanBuilder<'repo> {
    repo: &'repo Repository,
    options: ShortStatusOptions,
    cache_key: Option<StatusCacheKey>,
}

/// A prepared status/index work plan.
#[derive(Debug, Clone)]
pub struct StatusPlan<'repo> {
    repo: &'repo Repository,
    options: ShortStatusOptions,
    cache_key: Option<StatusCacheKey>,
}

impl Repository {
    /// Start a reusable status/index work plan.
    pub fn status_plan(&self) -> StatusPlanBuilder<'_> {
        StatusPlanBuilder {
            repo: self,
            options: ShortStatusOptions::default(),
            cache_key: None,
        }
    }
}

impl<'repo> StatusPlanBuilder<'repo> {
    /// Include or suppress untracked paths.
    pub fn include_untracked(mut self, include: bool) -> Self {
        self.options.untracked_mode = if include {
            StatusUntrackedMode::Normal
        } else {
            StatusUntrackedMode::None
        };
        self
    }

    pub fn untracked_mode(mut self, mode: StatusUntrackedMode) -> Self {
        self.options.untracked_mode = mode;
        self
    }

    pub fn include_ignored(mut self, include: bool) -> Self {
        self.options.include_ignored = include;
        self
    }

    pub fn reuse_index_cache(mut self, cache_key: impl Into<StatusCacheKey>) -> Self {
        self.cache_key = Some(cache_key.into());
        self
    }

    pub fn build(self) -> Result<StatusPlan<'repo>> {
        if self.repo.workdir().is_none() {
            return Err(GitError::Unsupported(
                "status plan requires a repository worktree".into(),
            ));
        }
        Ok(StatusPlan {
            repo: self.repo,
            options: self.options,
            cache_key: self.cache_key,
        })
    }
}

impl StatusPlan<'_> {
    pub fn options(&self) -> ShortStatusOptions {
        self.options
    }

    pub fn cache_key(&self) -> Option<&StatusCacheKey> {
        self.cache_key.as_ref()
    }

    pub fn collect(&self) -> Result<Vec<ShortStatusEntry>> {
        self.repo.short_status_with_options(self.options)
    }
}
