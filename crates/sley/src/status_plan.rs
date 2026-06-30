//! Reusable status/index work plans for embedders.

use crate::{
    GitError, ObjectId, Repository, Result, ShortStatusEntry, ShortStatusOptions, ShortStatusRow,
    StatusIgnoredMode, StatusUntrackedMode, StreamControl, SubmoduleStatus,
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

    pub fn ignored_mode(mut self, mode: StatusIgnoredMode) -> Self {
        self.options.ignored_mode = mode;
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

    /// Stream typed status rows without collecting them.
    pub fn stream<F>(&self, mut emit: F) -> Result<()>
    where
        F: for<'a> FnMut(StatusRow<'a>) -> Result<StreamControl>,
    {
        self.repo
            .stream_short_status_with_options(self.options, |row| emit(StatusRow::from_short(row)))
    }

    /// Visit typed status rows until the callback returns an error.
    pub fn for_each<F>(&self, mut visit: F) -> Result<()>
    where
        F: for<'a> FnMut(StatusRow<'a>) -> Result<()>,
    {
        self.stream(|row| {
            visit(row)?;
            Ok(StreamControl::Continue)
        })
    }

    /// Count status rows using the status engine's count path.
    pub fn count(&self) -> Result<usize> {
        self.repo.short_status_count_with_options(self.options)
    }

    /// Collect typed status rows.
    pub fn collect_rows(&self) -> Result<Vec<OwnedStatusRow>> {
        let mut rows = Vec::new();
        self.stream(|row| {
            rows.push(row.to_owned());
            Ok(StreamControl::Continue)
        })?;
        Ok(rows)
    }

    /// Collect compatibility short-status entries.
    pub fn collect(&self) -> Result<Vec<ShortStatusEntry>> {
        self.repo.short_status_with_options(self.options)
    }
}

/// Typed status code for one side of a short-status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    UpdatedButUnmerged,
    Untracked,
    Ignored,
    Other(u8),
}

impl StatusCode {
    pub fn from_short_code(code: u8) -> Self {
        match code {
            b' ' => Self::Unmodified,
            b'M' => Self::Modified,
            b'A' => Self::Added,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'T' => Self::TypeChanged,
            b'U' => Self::UpdatedButUnmerged,
            b'?' => Self::Untracked,
            b'!' => Self::Ignored,
            other => Self::Other(other),
        }
    }

    pub fn as_short_code(self) -> u8 {
        match self {
            Self::Unmodified => b' ',
            Self::Modified => b'M',
            Self::Added => b'A',
            Self::Deleted => b'D',
            Self::Renamed => b'R',
            Self::Copied => b'C',
            Self::TypeChanged => b'T',
            Self::UpdatedButUnmerged => b'U',
            Self::Untracked => b'?',
            Self::Ignored => b'!',
            Self::Other(code) => code,
        }
    }

    pub fn is_changed(self) -> bool {
        !matches!(self, Self::Unmodified)
    }
}

/// Borrowed typed status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRow<'a> {
    pub index: StatusCode,
    pub worktree: StatusCode,
    pub path: &'a [u8],
    pub head_mode: Option<u32>,
    pub index_mode: Option<u32>,
    pub worktree_mode: Option<u32>,
    pub head_oid: Option<ObjectId>,
    pub index_oid: Option<ObjectId>,
    pub submodule: Option<SubmoduleStatus>,
}

impl<'a> StatusRow<'a> {
    pub fn from_short(row: ShortStatusRow<'a>) -> Self {
        Self {
            index: StatusCode::from_short_code(row.index),
            worktree: StatusCode::from_short_code(row.worktree),
            path: row.path,
            head_mode: row.head_mode,
            index_mode: row.index_mode,
            worktree_mode: row.worktree_mode,
            head_oid: row.head_oid,
            index_oid: row.index_oid,
            submodule: row.submodule,
        }
    }

    pub fn to_owned(self) -> OwnedStatusRow {
        OwnedStatusRow {
            index: self.index,
            worktree: self.worktree,
            path: self.path.to_vec(),
            head_mode: self.head_mode,
            index_mode: self.index_mode,
            worktree_mode: self.worktree_mode,
            head_oid: self.head_oid,
            index_oid: self.index_oid,
            submodule: self.submodule,
        }
    }
}

/// Owned typed status row for callers that choose collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedStatusRow {
    pub index: StatusCode,
    pub worktree: StatusCode,
    pub path: Vec<u8>,
    pub head_mode: Option<u32>,
    pub index_mode: Option<u32>,
    pub worktree_mode: Option<u32>,
    pub head_oid: Option<ObjectId>,
    pub index_oid: Option<ObjectId>,
    pub submodule: Option<SubmoduleStatus>,
}
