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
