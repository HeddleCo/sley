//! Merge, rebase, pull, cherry-pick, revert, and merge-base commands.

use crate::commands::remote_cmds::{
    FetchRecurseSubmodules, FetchSubmoduleRequest, StdoutProgress, changed_gitlinks_for_fetch,
    fetch_bundle, fetch_populated_submodules_after_superproject, fetch_ref_snapshot,
    fetch_source_is_ssh, fetch_ssh_repository, ls_remote_git_dir, resolve_fetch_recurse_submodules,
};
use crate::*;
use sley_remote::FetchOptions;

mod merge;
mod merge_base;
mod merge_util;
mod pull;

pub(crate) use merge::{
    apply_merge_autostash, cmd_fmt_merge_msg, cmd_merge, cmd_merge_recursive,
    conclude_in_progress_merge, conclude_rebase_step_via_commit, directory_renames_config,
    effective_config_with_overrides, index_unmerged_paths, merge_rename_limit_config,
    parse_maybe_bool, print_branch_commit_summary, print_commit_shortstat_between_trees,
    read_merge_message_from_file, read_worktree_index, rebase_in_progress, save_merge_autostash,
    set_reflog_action_override, verify_fast_forward_untracked_safe,
};
pub(crate) use merge_base::{
    cmd_merge_base, commit_tree_oid, head_commit_oid, merge_base_fork_point, merge_bases,
    merge_bases_default_many,
};
pub(crate) use merge_util::{
    MergePathResult, MergePathResults, MergeTreeMap, RenameMergeConfig, clear_merge_df_blockers,
    merge_favor_from_strategy_opt, merge_favor_from_strategy_opts, merge_index_entry,
    merge_read_blob, merge_refuse_if_current_working_directory_becomes_file,
    merge_remove_worktree_file, merge_worktree_content, merge_write_worktree_file,
    three_way_merge_trees, three_way_merge_trees_inner_with_info,
    three_way_merge_trees_inner_with_info_opts_and_path_favor,
    three_way_merge_trees_inner_with_info_opts_and_path_resolvers, three_way_merge_trees_styled,
    three_way_merge_trees_with_favor, virtual_ancestor_entry_map, worktree_file_matches_ours,
};
pub(crate) use pull::{
    cmd_pull, fetch_head_merge_record, read_commit_tree, resolve_fetch_head_revision,
    update_merge_head_ref,
};
