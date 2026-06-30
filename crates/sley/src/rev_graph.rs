//! Revision graph facade for embedders.

use crate::{ObjectId, Repository, Result, StreamControl};

/// Repository-scoped revision graph helper.
#[derive(Debug, Clone, Copy)]
pub struct RevGraph<'repo> {
    repo: &'repo Repository,
}

/// One reachable commit emitted by [`RevGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachableCommit {
    pub oid: ObjectId,
    pub parents: Vec<ObjectId>,
    pub commit_time: i64,
}

/// Options for reachable commit walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReachableCommitOptions {
    first_parent: bool,
}

impl ReachableCommitOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn first_parent(mut self, first_parent: bool) -> Self {
        self.first_parent = first_parent;
        self
    }

    pub fn follows_first_parent(self) -> bool {
        self.first_parent
    }
}

impl Repository {
    /// Build a repository-scoped revision graph helper.
    pub fn rev_graph(&self) -> RevGraph<'_> {
        RevGraph { repo: self }
    }
}

impl RevGraph<'_> {
    /// Whether `ancestor` is reachable from `descendant` through parent links.
    pub fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        sley_rev::is_ancestor(
            self.repo.git_dir(),
            self.repo.object_format(),
            self.repo.objects().as_ref(),
            &ancestor,
            &descendant,
        )
    }

    /// Return `(ahead, behind)` commit counts for two commit tips.
    pub fn ahead_behind(&self, left: ObjectId, right: ObjectId) -> Result<(usize, usize)> {
        sley_rev::ahead_behind_counts(
            self.repo.git_dir(),
            self.repo.object_format(),
            self.repo.objects().as_ref(),
            &left,
            &right,
        )
    }

    /// Stream commits reachable from `tips` in the configured graph walk order.
    pub fn stream_reachable_commits<I, F>(
        &self,
        tips: I,
        options: ReachableCommitOptions,
        mut emit: F,
    ) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
        F: FnMut(ReachableCommit) -> Result<StreamControl>,
    {
        let objects = self.repo.objects();
        let mut walk = sley_rev::RevWalk::new(
            self.repo.git_dir(),
            self.repo.object_format(),
            objects.as_ref(),
            tips,
        )
        .first_parent(options.first_parent);
        while let Some(commit) = walk.try_next()? {
            if matches!(emit(commit.into())?, StreamControl::Stop) {
                break;
            }
        }
        Ok(())
    }

    /// Collect commits reachable from `tips`.
    pub fn collect_reachable_commits<I>(
        &self,
        tips: I,
        options: ReachableCommitOptions,
    ) -> Result<Vec<ReachableCommit>>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut commits = Vec::new();
        self.stream_reachable_commits(tips, options, |commit| {
            commits.push(commit);
            Ok(StreamControl::Continue)
        })?;
        Ok(commits)
    }
}

impl From<sley_rev::CommitMetadata> for ReachableCommit {
    fn from(value: sley_rev::CommitMetadata) -> Self {
        Self {
            oid: value.oid,
            parents: value.parents,
            commit_time: value.commit_time,
        }
    }
}
