//! `git branch` and all its modes
//! (list/create/delete/rename/copy/set-upstream/edit-description).

#[path = "../branch_options.rs"]
mod branch_options;

mod config;
mod create;
mod delete;
mod dispatch;
mod list;
mod move_;
mod operand;
mod positional;
mod positional_table;
mod upstream;

pub(super) struct BranchCommandContext {
    pub(super) repository: sley::Repository,
    pub(super) refs: crate::FileRefStore,
    pub(super) config: crate::GitConfig,
    pub(super) replace_objects: bool,
}

impl BranchCommandContext {
    pub(super) fn open(session: &crate::session::CliSession) -> crate::Result<Self> {
        let repository = session.open_repository()?;
        let refs = repository.references();
        // Layer `-c` / `--config-env` overrides on top of the file stack so
        // `git -c submodule.propagateBranches=false branch --recurse-submodules`
        // (t3207) and `git -c submodule.recurse=true branch` are honoured.
        // `config_snapshot` deliberately omits command-line parameters.
        let worktree = repository
            .workdir()
            .unwrap_or_else(|| session.cwd().to_path_buf());
        let config =
            crate::commands::remote::read_effective_repo_config(repository.git_dir(), &worktree)?;
        Ok(Self {
            repository,
            refs,
            config,
            replace_objects: session.replace_objects(),
        })
    }

    pub(super) fn git_dir(&self) -> &std::path::Path {
        self.repository.git_dir()
    }

    pub(super) fn format(&self) -> crate::ObjectFormat {
        self.repository.object_format()
    }

    pub(super) fn objects(&self) -> &crate::sley_odb::FileObjectDatabase {
        self.repository.object_database()
    }
}

// Names in scope for branch_options.rs (`use super::{...}`).
use create::BranchCreateOptions;
use delete::{BranchDeleteMode, BranchDeleteOptions};
use list::{
    BranchColumnStyle, BranchFormatListOptions, BranchGeneralListOptions, BranchListFilters,
    BranchListMode, BranchSort, BranchVerboseListOptions, branch_ahead_behind_sort_value,
    branch_contains_eq_value, branch_date_sort_value, branch_merged_eq_value,
    branch_no_contains_eq_value, branch_no_merged_eq_value, branch_objectname_sort_value,
    branch_objectsize_sort_value, branch_objecttype_sort_value, branch_push_sort_value,
    branch_upstream_sort_value, branch_version_sort_value,
};
use move_::{BranchMoveKind, BranchMoveOptions};
use upstream::{BranchUpstreamAction, BranchUpstreamOptions};

pub(crate) use create::{BranchTrackMode, branch_create_set_tracking, create_branch_from_start};
pub(crate) use dispatch::cmd_branch;
