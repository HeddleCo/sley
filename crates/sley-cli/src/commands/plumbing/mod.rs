//! Plumbing commands extracted from the monolithic `plumbing.rs` (W61).

mod add;
mod apply;
mod archive;
mod bundle;
mod clean;
mod commit_graph;
mod commit_tree;
mod fsck;
mod init;
mod prune_packed;
mod rerere;
mod replace;
mod worktree;

#[path = "../plumbing_options.rs"]
mod plumbing_options;

pub(crate) use add::cmd_add;
pub(crate) use apply::{apply_binary_outcome, BinaryApply, cmd_apply};
pub(crate) use archive::cmd_archive;
pub(crate) use bundle::cmd_bundle;
pub(crate) use clean::cmd_clean;
pub(crate) use commit_graph::cmd_commit_graph;
pub(crate) use commit_tree::cmd_commit_tree;
pub(crate) use fsck::{cmd_fsck, repo_has_promisor_remote};
pub(crate) use init::cmd_init;
pub(crate) use prune_packed::cmd_prune_packed;
pub(crate) use replace::cmd_replace;
pub(crate) use worktree::{cmd_mv, cmd_rm};