//! Positional / legacy argv patterns for `git branch`.
//!
//! This module is the W60 mechanical argv `match` (~5.9k LOC). Each arm uses
//! guard-then-`.expect()` after `is_some()` checks; the module-level
//! `expect_used` allow is intentional until the table lands post-W90.
//!
//! **Post-W90 table-driven migration path** (do not rewrite wholesale before
//! the parity gate):
//!
//! 1. **Extract shared permutations** — remote/all list + noop-display arms
//!    already route through [`super::positional_table`] and
//!    [`branch_remote_or_all_mode_unchecked`]; extend that table for the next
//!    highest-churn clusters (color/column/abbrev noop flags, create/delete).
//! 2. **Introduce an argv pattern table** — keyed by `(argc, token classes)`
//!    with small dispatch closures; keep `match` as a thin router over table
//!    hits until coverage is complete.
//! 3. **Burn down `expect_used`** — replace guard+`.expect()` with table
//!    entries that encode the invariant; drop this module's `#![allow]` once
//!    clippy is clean.
//!
//! See also [`super::positional_table`] and plan §12 / W72.

#![allow(clippy::expect_used)]

use super::create::{branch_create_set_tracking, create_branch_from_start};
use super::delete::{delete_merged_branches, force_delete_branches, force_update_branch};
use super::list::*;
use super::positional_table::{
    RemoteOrAllNoopDisplayCount, dispatch_remote_or_all_noop_display,
};
use crate::*;

pub(super) fn dispatch_branch_positional_args(
    git_dir: &Path,
    format: ObjectFormat,
    store: &FileRefStore,
    args: &[String],
) -> Result<()> {
    match args {
        [] => print_branch_list(store, BranchListMode::Local),
        [flag] if flag == "--list" => print_branch_list(store, BranchListMode::Local),
        [flag] if flag == "-r" || flag == "--remotes" => {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag] if flag == "-a" || flag == "--all" => print_branch_list(store, BranchListMode::All),
        [flag] if flag == "--color" || flag == "--color=always" => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [color, no_color] if branch_color_always_flag(color) && no_color == "--no-color" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_color, color] if no_color == "--no-color" && branch_color_always_flag(color) => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [flag, color]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Remote)
        }
        [color, flag]
            if (flag == "-r" || flag == "--remotes")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Remote)
        }
        [flag, color]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::All)
        }
        [color, flag]
            if (flag == "-a" || flag == "--all")
                && (color == "--color" || color == "--color=always") =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::All)
        }
        [flag, color, no_color]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            dispatch_remote_or_all_noop_display(store, flag, RemoteOrAllNoopDisplayCount::Two)
        }
        [flag, no_color, color]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_colored_remote_or_all_flag(git_dir, store, flag)
        }
        [flag, display_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag) =>
        {
            dispatch_remote_or_all_noop_display(store, flag, RemoteOrAllNoopDisplayCount::One)
        }
        [display_flag, flag]
            if branch_list_noop_display_flag(display_flag)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            dispatch_remote_or_all_noop_display(store, flag, RemoteOrAllNoopDisplayCount::One)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            dispatch_remote_or_all_noop_display(store, flag, RemoteOrAllNoopDisplayCount::Two)
        }
        [first, second, flag]
            if branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            dispatch_remote_or_all_noop_display(store, flag, RemoteOrAllNoopDisplayCount::Two)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [first, second, flag]
            if branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, first, second]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [first, second, flag]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [sort, flag]
            if branch_version_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [sort, flag]
            if branch_objectname_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [sort, flag]
            if branch_objecttype_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [sort, flag]
            if branch_objectsize_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [sort, flag]
            if branch_date_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [sort, flag]
            if branch_upstream_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [sort, flag]
            if branch_push_sort_value(sort).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [sort, key, flag]
            if sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort]
            if branch_remote_or_all_mode(flag).is_some() && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [sort, flag]
            if sort == "--sort=-refname" && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "-refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, key, no_sort]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(git_dir, format, store, mode, field, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, no_sort, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [sort, key, flag]
            if sort == "--sort"
                && key == "refname"
                && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag] if branch_ignore_case_flag(flag) => print_branch_list(store, BranchListMode::Local),
        [list, flag] if list == "--list" && branch_ignore_case_flag(flag) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, list] if branch_ignore_case_flag(flag) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, ignore]
            if branch_remote_or_all_mode(flag).is_some() && branch_ignore_case_flag(ignore) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [ignore, flag]
            if branch_ignore_case_flag(ignore) && branch_remote_or_all_mode(flag).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag] if flag == "--no-points-at" => print_branch_list(store, BranchListMode::Local),
        [points_at, _rev, no_points_at]
            if points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_points_at, points_at, rev]
            if no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [points_at, no_points_at]
            if points_at.starts_with("--points-at=") && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_points_at, points_at]
            if no_points_at == "--no-points-at" && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag, color] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [color, flag] if flag == "--list" && (color == "--color" || color == "--color=always") => {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [list, color, no_color]
            if list == "--list" && branch_color_always_flag(color) && no_color == "--no-color" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [color, no_color, list, patterns @ ..]
            if branch_color_always_flag(color) && no_color == "--no-color" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, color, no_color, patterns @ ..]
            if list == "--list" && branch_color_always_flag(color) && no_color == "--no-color" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, no_color, color]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_colored(git_dir, store, BranchListMode::Local)
        }
        [list, no_color, color, patterns @ ..]
            if list == "--list" && no_color == "--no-color" && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [no_color, color, list, patterns @ ..]
            if no_color == "--no-color" && branch_color_always_flag(color) && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_always_flag(color) => {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [color, list, patterns @ ..] if branch_color_always_flag(color) && list == "--list" => {
            print_branch_list_matching_colored(store, BranchListMode::Local, patterns)
        }
        [list, color] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, color, patterns @ ..] if list == "--list" && branch_color_noop_flag(color) => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [color, list] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [color, list, patterns @ ..] if branch_color_noop_flag(color) && list == "--list" => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, points_at, _rev, no_points_at]
            if list == "--list" && points_at == "--points-at" && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, no_points_at, points_at, rev]
            if list == "--list" && no_points_at == "--no-points-at" && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [list, points_at, no_points_at]
            if list == "--list"
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, no_points_at, points_at]
            if list == "--list"
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [list, display_flag]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, display_flag, patterns @ ..]
            if list == "--list" && branch_list_noop_display_flag(display_flag) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [display_flag, list]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [display_flag, list, patterns @ ..]
            if branch_list_noop_display_flag(display_flag) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, sort] if list == "--list" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [list, sort] if list == "--list" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort] if list == "--list" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, key]
            if list == "--list" && sort == "--sort" && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objectname_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objecttype_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_objectsize_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_date_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_upstream_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_push_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list"
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..]
            if list == "--list"
                && branch_version_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [list, sort] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [list, sort, key] if list == "--list" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=-refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, patterns @ ..] if list == "--list" && sort == "--sort=-refname" => {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "-refname" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [list, sort, key, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, list] if sort == "--sort" && key == "refname" && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, list] if branch_version_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objectname_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_objecttype_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_objectsize_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [sort, list] if branch_date_sort_value(sort).is_some() && list == "--list" => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [sort, list] if branch_upstream_sort_value(sort).is_some() && list == "--list" => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, list] if branch_push_sort_value(sort).is_some() && list == "--list" => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, key, list]
            if sort == "--sort" && branch_push_sort_value(key).is_some() && list == "--list" =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [sort, list, patterns @ ..]
            if branch_objectname_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_objecttype_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_objectsize_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_date_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [sort, list, patterns @ ..]
            if branch_upstream_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_push_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                (field, descending),
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir,
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list, patterns @ ..]
            if branch_version_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                descending,
            )
        }
        [sort, list] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, key, list] if sort == "--sort" && key == "-refname" && list == "--list" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, list, patterns @ ..] if sort == "--sort=-refname" && list == "--list" => {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort" && key == "-refname" && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort=-refname" && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            print_branch_list_matching_sorted(
                store,
                BranchListMode::Local,
                patterns,
                false,
                true,
            )
        }
        [sort, key, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, no_sort, list, patterns @ ..]
            if sort == "--sort=-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort=refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort=refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [sort, key, no_sort, list, patterns @ ..]
            if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_sort, sort, key, list, patterns @ ..]
            if no_sort == "--no-sort" && sort == "--sort" && key == "refname" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, sort, key, no_sort, patterns @ ..]
            if list == "--list" && sort == "--sort" && key == "refname" && no_sort == "--no-sort" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, list, color, no_color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_color_always_flag(color)
                && no_color == "--no-color" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Remote, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::Remote, patterns)
        }
        [flag, list, color, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_color_always_flag(color) =>
        {
            print_branch_list_matching_colored(store, BranchListMode::All, patterns)
        }
        [flag, color, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            print_branch_list_matching_colored(store, BranchListMode::All, patterns)
        }
        [flag, color, no_color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_color_always_flag(color)
                && no_color == "--no-color"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, no_color, color, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_color == "--no-color"
                && branch_color_always_flag(color) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_colored(store, mode, patterns)
        }
        [flag, no_color, color, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_color == "--no-color"
                && branch_color_always_flag(color)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_colored(store, mode, patterns)
        }
        [flag, rev] if flag == "--points-at" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag, rev] if flag == "--contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [contains, contains_rev, no_contains, no_contains_rev]
            if contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [no_contains, no_contains_rev, contains, contains_rev]
            if no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag] if flag == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, rev] if flag == "--merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [merged, merged_rev, no_merged, no_merged_rev]
            if merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [no_merged, no_merged_rev, merged, merged_rev]
            if no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, rev] if flag == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, rev, patterns @ ..] if flag == "--list" && points_at == "--points-at" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, BranchListMode::Local, &oid, patterns)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--contains"
                && patterns
                    .first()
                    .is_none_or(|value| *value != "--no-contains") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if list == "--list" && contains == "--contains" && no_contains == "--no-contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, no_contains, no_contains_rev, contains, contains_rev, patterns @ ..]
            if list == "--list" && no_contains == "--no-contains" && contains == "--contains" =>
        {
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, contains, rev, patterns @ ..]
            if flag == "--list"
                && contains == "--no-contains"
                && patterns.first().is_none_or(|value| *value != "--contains") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, contains] if flag == "--list" && contains == "--no-contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--merged"
                && patterns.first().is_none_or(|value| *value != "--no-merged") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [list, merged, merged_rev, no_merged, no_merged_rev, patterns @ ..]
            if list == "--list" && merged == "--merged" && no_merged == "--no-merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [list, no_merged, no_merged_rev, merged, merged_rev, patterns @ ..]
            if list == "--list" && no_merged == "--no-merged" && merged == "--merged" =>
        {
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, merged] if flag == "--list" && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, merged, rev, patterns @ ..]
            if flag == "--list"
                && merged == "--no-merged"
                && patterns.first().is_none_or(|value| *value != "--merged") =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, points_at, rev]
            if (flag == "-r" || flag == "--remotes") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains, contains_rev, no_contains, no_contains_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && contains == "--contains"
                && no_contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, no_contains, no_contains_rev, contains, contains_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && no_contains == "--no-contains"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains, rev]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--merged" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged, merged_rev, no_merged, no_merged_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && merged == "--merged"
                && no_merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, no_merged, no_merged_rev, merged, merged_rev]
            if branch_remote_or_all_mode(flag).is_some()
                && no_merged == "--no-merged"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, merged] if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, merged, rev]
            if (flag == "-r" || flag == "--remotes") && merged == "--no-merged" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at, rev]
            if (flag == "-a" || flag == "--all") && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, points_at, _rev, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at == "--points-at"
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::All)
        }
        [flag, no_points_at, points_at, rev]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at == "--points-at" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, contains, rev] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains] if (flag == "-a" || flag == "--all") && contains == "--contains" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains, rev]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains == "--no-contains" =>
        {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, "HEAD")?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [flag, merged, rev] if (flag == "-a" || flag == "--all") && merged == "--no-merged" => {
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [contains, no_contains]
            if branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [no_contains, contains]
            if branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [merged, no_merged]
            if branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [no_merged, merged]
            if branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag] if flag.starts_with("--points-at=") => {
            let rev = flag
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Local, &oid)
        }
        [flag] if flag.starts_with("--contains=") => {
            let rev = flag
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-contains=") => {
            let rev = flag
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &oid,
                false,
            )
        }
        [flag] if flag.starts_with("--merged=") => {
            let rev = flag
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, true)
        }
        [flag] if flag.starts_with("--no-merged=") => {
            let rev = flag
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Local, &oid, false)
        }
        [flag, points_at, patterns @ ..] if flag == "--list" && points_at.starts_with("--points-at=") => {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, BranchListMode::Local, &oid, patterns)
        }
        [list, contains, no_contains, patterns @ ..]
            if list == "--list"
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, no_contains, contains, patterns @ ..]
            if list == "--list"
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [list, merged, no_merged, patterns @ ..]
            if list == "--list"
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [list, no_merged, merged, patterns @ ..]
            if list == "--list"
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--contains=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, contains, patterns @ ..] if flag == "--list" && contains.starts_with("--no-contains=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--merged=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, merged, patterns @ ..] if flag == "--list" && merged.starts_with("--no-merged=") => {
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                &[oid],
                patterns,
            )
        }
        [format_flag, ignore, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [ignore, format_flag, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [list, ignore, format_flag, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [format_flag, ignore, reset, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [format_flag, format_spec, ignore, list, patterns @ ..]
            if format_flag == "--format" && branch_ignore_case_enabled_flag(ignore) && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [ignore, format_flag, format_spec, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore) && format_flag == "--format" && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [list, ignore, format_flag, format_spec, patterns @ ..]
            if list == "--list" && branch_ignore_case_enabled_flag(ignore) && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                true,
                format_spec,
            )
        }
        [format_flag, format_spec, ignore, reset, list, patterns @ ..]
            if format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, ignore, reset, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [ignore, list, reset, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [ignore, reset, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, list, patterns @ ..]
            if branch_ignore_case_enabled_flag(flag) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, true)
        }
        [list, flag, reset, patterns @ ..]
            if list == "--list"
                && branch_ignore_case_enabled_flag(flag)
                && reset == "--no-ignore-case" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, flag, patterns @ ..]
            if list == "--list" && branch_ignore_case_enabled_flag(flag) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, true)
        }
        [list, column] if list == "--list" && branch_column_noop_flag(column) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [list, column, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(column) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [column, list] if branch_column_noop_flag(column) && list == "--list" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [column, list, patterns @ ..]
            if branch_column_noop_flag(column) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_column_noop_flag(first) && branch_column_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_column_noop_flag(first) && branch_column_noop_flag(second) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [first, second, list, patterns @ ..]
            if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, first, second, patterns @ ..]
            if list == "--list" && branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, no_format, list, patterns @ ..]
            if format_flag.starts_with("--format=") && no_format == "--no-format" && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, format_spec, no_format, list, patterns @ ..]
            if format_flag == "--format" && no_format == "--no-format" && list == "--list" =>
        {
            let _ = format_spec;
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [no_format, format_flag, list, patterns @ ..]
            if no_format == "--no-format" && format_flag.starts_with("--format=") && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [no_format, format_flag, format_spec, list, patterns @ ..]
            if no_format == "--no-format" && format_flag == "--format" && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, format_flag, no_format, patterns @ ..]
            if list == "--list" && format_flag.starts_with("--format=") && no_format == "--no-format" =>
        {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [list, format_flag, format_spec, no_format, patterns @ ..]
            if list == "--list" && format_flag == "--format" && no_format == "--no-format" =>
        {
            let _ = format_spec;
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [format_flag, omit_empty, list, patterns @ ..]
            if format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, format_flag, omit_empty, patterns @ ..]
            if list == "--list"
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [format_flag, format_spec, omit_empty, list, patterns @ ..]
            if format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, format_flag, format_spec, omit_empty, patterns @ ..]
            if list == "--list"
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [list, flag, patterns @ ..] if list == "--list" && flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_spec, list, patterns @ ..] if flag == "--format" && list == "--list" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [list, flag, format_spec, patterns @ ..] if list == "--list" && flag == "--format" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, patterns @ ..] if flag == "--list" => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
        [flag, points_at]
            if (flag == "-r" || flag == "--remotes") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-r" || flag == "--remotes")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::Remote)
        }
        [flag, no_points_at, points_at]
            if (flag == "-r" || flag == "--remotes")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::Remote, &oid)
        }
        [flag, contains, no_contains]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, no_contains, contains]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
            )
        }
        [flag, merged, no_merged]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, no_merged, merged]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains.starts_with("--contains=") =>
        {
            let rev = contains
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                true,
            )
        }
        [flag, contains]
            if (flag == "-r" || flag == "--remotes") && contains.starts_with("--no-contains=") =>
        {
            let rev = contains
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &oid,
                false,
            )
        }
        [flag, merged]
            if (flag == "-r" || flag == "--remotes") && merged.starts_with("--merged=") =>
        {
            let rev = merged
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, true)
        }
        [flag, merged]
            if (flag == "-r" || flag == "--remotes") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::Remote, &oid, false)
        }
        [flag, points_at]
            if (flag == "-a" || flag == "--all") && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, points_at, no_points_at]
            if (flag == "-a" || flag == "--all")
                && points_at.starts_with("--points-at=")
                && no_points_at == "--no-points-at" =>
        {
            print_branch_list(store, BranchListMode::All)
        }
        [flag, no_points_at, points_at]
            if (flag == "-a" || flag == "--all")
                && no_points_at == "--no-points-at"
                && points_at.starts_with("--points-at=") =>
        {
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at(store, BranchListMode::All, &oid)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--contains=") =>
        {
            let rev = contains
                .strip_prefix("--contains=")
                .ok_or_else(|| GitError::Command("branch --contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, contains]
            if (flag == "-a" || flag == "--all") && contains.starts_with("--no-contains=") =>
        {
            let rev = contains
                .strip_prefix("--no-contains=")
                .ok_or_else(|| GitError::Command("branch --no-contains requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &oid,
                false,
            )
        }
        [flag, merged]
            if (flag == "-a" || flag == "--all") && merged.starts_with("--merged=") =>
        {
            let rev = merged
                .strip_prefix("--merged=")
                .ok_or_else(|| GitError::Command("branch --merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, true)
        }
        [flag, merged]
            if (flag == "-a" || flag == "--all") && merged.starts_with("--no-merged=") =>
        {
            let rev = merged
                .strip_prefix("--no-merged=")
                .ok_or_else(|| GitError::Command("branch --no-merged requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged(git_dir, format, store, BranchListMode::All, &oid, false)
        }
        [flag, format_flag, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, format_flag, format_spec, no_format]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, no_format, format_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag.starts_with("--format=") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, &[], false, format_spec)
        }
        [flag, no_format, format_flag, format_spec]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format(git_dir, format, store, mode, &[], false, format_spec)
        }
        [flag, format_flag, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, format_spec, no_format, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && no_format == "--no-format"
                && list == "--list" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_format, format_flag, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, no_format, format_flag, format_spec, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_format == "--no-format"
                && format_flag == "--format"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, list, format_flag, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag.starts_with("--format=")
                && no_format == "--no-format" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, format_flag, format_spec, no_format, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag == "--format"
                && no_format == "--no-format" =>
        {
            let _ = format_spec;
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, omit_empty]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, omit_empty, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, list, format_flag, omit_empty, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag.starts_with("--format=")
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, format_spec, omit_empty]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag, format_spec, omit_empty, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, list, format_flag, format_spec, omit_empty, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && format_flag == "--format"
                && branch_omit_empty_value(omit_empty).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode,
                    patterns,
                    ignore_case: false,
                    format_spec,
                    omit_empty: branch_omit_empty_value(omit_empty).expect("guard checked flag"),
                },
            )
        }
        [flag, format_flag]
            if (flag == "-r" || flag == "--remotes") && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec]
            if (flag == "-r" || flag == "--remotes") && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && format_flag == "--format"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, format_spec, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Remote,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag]
            if (flag == "-a" || flag == "--all") && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec]
            if (flag == "-a" || flag == "--all") && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                &[],
                false,
                format_spec,
            )
        }
        [flag, format_flag, format_spec, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && format_flag == "--format"
                && list == "--list" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, format_flag, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && format_flag.starts_with("--format=")
                && list == "--list" =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, format_spec, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && format_flag == "--format" =>
        {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, format_flag, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && format_flag.starts_with("--format=") =>
        {
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::All,
                patterns,
                false,
                format_spec,
            )
        }
        [flag, list, display_flag]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, list, display_flag, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_list_noop_display_flag(display_flag) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_column_noop_flag(first)
                && branch_column_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_abbrev_noop_flag(first)
                && branch_abbrev_noop_flag(second) =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, display_flag, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, display_flag, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_list_noop_display_flag(display_flag)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, first, second, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, first, second, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_version_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectname_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objecttype_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectsize_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_upstream_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_push_sort_value(sort).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectname_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objecttype_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_objectsize_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_upstream_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_push_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_version_sort_value(sort).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, list, sort]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, list, sort, key]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "-refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, list, sort, key, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list(store, mode)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(git_dir, format, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, mode, descending)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_push_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_version_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectname_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objecttype_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_objectsize_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_date_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                mode,
                patterns,
                false,
                (field, descending),
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_upstream_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && branch_push_sort_value(sort).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectname_sorted(store, mode, patterns, false, descending)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objecttype_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_objectsize_sorted(
                git_dir, format, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_date_sorted(
                git_dir,
                format,
                store,
                mode,
                patterns,
                false,
                (field, descending),
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_upstream_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some()
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_push_sorted(
                git_dir, store, mode, patterns, false, descending,
            )
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_version_sort_value(sort).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && branch_version_sort_value(key).is_some()
                && list == "--list"
                && patterns.first().is_none_or(|value| *value != "--no-sort") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let descending =
                branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_matching_version_sorted(store, mode, patterns, false, descending)
        }
        [flag, sort, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, key, list]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_sorted(store, mode, true)
        }
        [flag, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "-refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching_sorted(store, mode, patterns, false, true)
        }
        [flag, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort=-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_sort, sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort=refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort=refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, sort, key, no_sort, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && sort == "--sort"
                && key == "-refname"
                && no_sort == "--no-sort"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, no_sort, sort, key, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && no_sort == "--no-sort"
                && sort == "--sort"
                && key == "refname"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, sort, key, no_sort, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && sort == "--sort"
                && key == "refname"
                && no_sort == "--no-sort" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, format_flag, ignore, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, list, ignore, format_flag, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag.starts_with("--format=") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, format_flag, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag.starts_with("--format=")
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let format_spec = format_flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, format_flag, format_spec, ignore, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, list, ignore, format_flag, format_spec, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && format_flag == "--format" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format(git_dir, format, store, mode, patterns, true, format_spec)
        }
        [flag, format_flag, format_spec, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && format_flag == "--format"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_format(git_dir, format, store, mode, patterns, false, format_spec)
        }
        [flag, list, ignore, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, ignore, list, reset, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list"
                && reset == "--no-ignore-case" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, ignore, reset, list, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && branch_ignore_case_enabled_flag(ignore)
                && reset == "--no-ignore-case"
                && list == "--list" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            print_branch_list_matching(store, mode, patterns, false)
        }
        [flag, list, points_at, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && points_at == "--points-at" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, mode, &oid, patterns)
        }
        [flag, list, points_at, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && points_at.starts_with("--points-at=") =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let rev = points_at
                .strip_prefix("--points-at=")
                .ok_or_else(|| GitError::Command("branch --points-at requires a value".into()))?;
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_points_at_matching(store, mode, &oid, patterns)
        }
        [flag, list, contains, contains_rev, no_contains, no_contains_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--contains"
                && no_contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, no_contains, no_contains_rev, contains, contains_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_contains == "--no-contains"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(git_dir, format, contains_rev)?;
            let no_contains_oid = resolve_revision(git_dir, format, no_contains_rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, merged, merged_rev, no_merged, no_merged_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--merged"
                && no_merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, no_merged, no_merged_rev, merged, merged_rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && no_merged == "--no-merged"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(git_dir, format, merged_rev)?;
            let no_merged_oid = resolve_revision(git_dir, format, no_merged_rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, contains, no_contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_contains_eq_value(contains).is_some()
                && branch_no_contains_eq_value(no_contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, no_contains, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_contains_eq_value(no_contains).is_some()
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let contains_oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            let no_contains_oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(no_contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[contains_oid],
                &[no_contains_oid],
                patterns,
            )
        }
        [flag, list, merged, no_merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_merged_eq_value(merged).is_some()
                && branch_no_merged_eq_value(no_merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, no_merged, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_merged_eq_value(no_merged).is_some()
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let merged_oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            let no_merged_oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(no_merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[merged_oid],
                &[no_merged_oid],
                patterns,
            )
        }
        [flag, list, contains, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, contains, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && contains == "--no-contains" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, merged, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, merged, rev, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && merged == "--no-merged" =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(git_dir, format, rev)?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(
                git_dir,
                format,
                branch_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, contains, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_contains_eq_value(contains).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_contains_eq_value(contains).expect("guard checked branch option"),
            )?;
            print_branch_list_contains_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, list, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(
                git_dir,
                format,
                branch_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[oid],
                &[],
                patterns,
            )
        }
        [flag, list, merged, patterns @ ..]
            if branch_remote_or_all_mode(flag).is_some()
                && list == "--list"
                && branch_no_merged_eq_value(merged).is_some() =>
        {
            let mode = branch_remote_or_all_mode_unchecked(flag);
            let oid = resolve_revision(
                git_dir,
                format,
                branch_no_merged_eq_value(merged).expect("guard checked branch option"),
            )?;
            print_branch_list_merged_filters_matching(
                git_dir,
                format,
                store,
                mode,
                &[],
                &[oid],
                patterns,
            )
        }
        [flag, ignore, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-r" || flag == "--remotes")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, true)
        }
        [flag, list, patterns @ ..]
            if (flag == "-r" || flag == "--remotes") && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::Remote, patterns, false)
        }
        [flag, ignore, list, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && branch_ignore_case_enabled_flag(ignore)
                && list == "--list" =>
        {
            print_branch_list_matching(store, BranchListMode::All, patterns, true)
        }
        [flag, list, ignore, patterns @ ..]
            if (flag == "-a" || flag == "--all")
                && list == "--list"
                && branch_ignore_case_enabled_flag(ignore) =>
        {
            print_branch_list_matching(store, BranchListMode::All, patterns, true)
        }
        [flag, list, patterns @ ..] if (flag == "-a" || flag == "--all") && list == "--list" => {
            print_branch_list_matching(store, BranchListMode::All, patterns, false)
        }
        [flag, key] if flag == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag] if flag.starts_with("--sort=") && branch_version_sort_value(flag).is_some() => {
            let descending = branch_version_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_version_sort_value(key).is_some() => {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [flag] if flag.starts_with("--sort=") && branch_objectname_sort_value(flag).is_some() => {
            let descending =
                branch_objectname_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_objectname_sort_value(key).is_some() => {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [flag] if flag.starts_with("--sort=") && branch_objecttype_sort_value(flag).is_some() => {
            let descending =
                branch_objecttype_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objecttype_sort_value(key).is_some() => {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if flag.starts_with("--sort=") && branch_objectsize_sort_value(flag).is_some() => {
            let descending =
                branch_objectsize_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_objectsize_sort_value(key).is_some() => {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [flag] if flag.starts_with("--sort=") && branch_date_sort_value(flag).is_some() => {
            let (field, descending) =
                branch_date_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag, key] if flag == "--sort" && branch_date_sort_value(key).is_some() => {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [flag] if flag.starts_with("--sort=") && branch_upstream_sort_value(flag).is_some() => {
            let descending =
                branch_upstream_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag] if flag.starts_with("--sort=") && branch_push_sort_value(flag).is_some() => {
            let descending = branch_push_sort_value(flag).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_upstream_sort_value(key).is_some() => {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag, key] if flag == "--sort" && branch_push_sort_value(key).is_some() => {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [flag] if flag == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [flag, key] if flag == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, no_sort] if sort == "--sort=refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, no_sort]
            if (branch_non_refname_sort_value(sort))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, no_sort] if sort == "--sort=-refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_version_sort_value(sort).is_some() => {
            let descending = branch_version_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectname_sort_value(sort).is_some() => {
            let descending =
                branch_objectname_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objecttype_sort_value(sort).is_some() => {
            let descending =
                branch_objecttype_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_objectsize_sort_value(sort).is_some() => {
            let descending =
                branch_objectsize_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_date_sort_value(sort).is_some() => {
            let (field, descending) =
                branch_date_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_upstream_sort_value(sort).is_some() => {
            let descending =
                branch_upstream_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && branch_push_sort_value(sort).is_some() => {
            let descending = branch_push_sort_value(sort).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort] if no_sort == "--no-sort" && sort == "--sort=-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, key, no_sort]
            if sort == "--sort"
                && (branch_non_refname_sort_value(key))
                && no_sort == "--no-sort" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [sort, key, no_sort] if sort == "--sort" && key == "-refname" && no_sort == "--no-sort" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "refname" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort" && sort == "--sort" && branch_version_sort_value(key).is_some() =>
        {
            let descending = branch_version_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_version_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectname_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectname_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectname_sorted(store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objecttype_sort_value(key).is_some() =>
        {
            let descending =
                branch_objecttype_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objecttype_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_objectsize_sort_value(key).is_some() =>
        {
            let descending =
                branch_objectsize_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_objectsize_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_date_sort_value(key).is_some() =>
        {
            let (field, descending) =
                branch_date_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_date_sorted(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                field,
                descending,
            )
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_upstream_sort_value(key).is_some() =>
        {
            let descending =
                branch_upstream_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_upstream_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key]
            if no_sort == "--no-sort"
                && sort == "--sort"
                && branch_push_sort_value(key).is_some() =>
        {
            let descending = branch_push_sort_value(key).expect("guard checked branch sort value");
            print_branch_list_push_sorted(git_dir, store, BranchListMode::Local, descending)
        }
        [no_sort, sort, key] if no_sort == "--no-sort" && sort == "--sort" && key == "-refname" => {
            print_branch_list_sorted(store, BranchListMode::Local, true)
        }
        [first, second]
            if branch_omit_empty_value(first).is_some()
                && branch_omit_empty_value(second).is_some() =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [first, second] if branch_column_noop_flag(first) && branch_column_noop_flag(second) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [first, second] if branch_abbrev_noop_flag(first) && branch_abbrev_noop_flag(second) => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, no_format] if flag.starts_with("--format=") && no_format == "--no-format" => {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag, format_spec, no_format] if flag == "--format" && no_format == "--no-format" => {
            let _ = format_spec;
            print_branch_list(store, BranchListMode::Local)
        }
        [no_format, flag] if no_format == "--no-format" && flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                false,
                format_spec,
            )
        }
        [no_format, flag, format_spec] if no_format == "--no-format" && flag == "--format" => {
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                &[],
                false,
                format_spec,
            )
        }
        [flag, omit_empty]
            if flag.starts_with("--format=")
                && (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty") =>
        {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [omit_empty, flag]
            if (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty")
                && flag.starts_with("--format=") =>
        {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [flag, format_spec, omit_empty]
            if flag == "--format"
                && (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty") =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [omit_empty, flag, format_spec]
            if (omit_empty == "--omit-empty" || omit_empty == "--no-omit-empty")
                && flag == "--format" =>
        {
            print_branch_list_format_omit_empty(
                git_dir,
                format,
                store,
                BranchFormatPrintOptions {
                    mode: BranchListMode::Local,
                    patterns: &[],
                    ignore_case: false,
                    format_spec,
                    omit_empty: omit_empty == "--omit-empty",
                },
            )
        }
        [flag] if flag.starts_with("--format=") => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(git_dir, format, store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, format_spec] if flag == "--format" => {
            print_branch_list_format(git_dir, format, store, BranchListMode::Local, &[], false, format_spec)
        }
        [flag, list, patterns @ ..] if flag.starts_with("--format=") && list == "--list" => {
            let format_spec = flag
                .strip_prefix("--format=")
                .ok_or_else(|| GitError::Command("branch --format requires a value".into()))?;
            print_branch_list_format(
                git_dir,
                format,
                store,
                BranchListMode::Local,
                patterns,
                false,
                format_spec,
            )
        }
        [flag]
            if flag == "--no-color"
                || flag == "--color=never"
                || flag == "--color=auto"
                || branch_column_noop_flag(flag)
                || flag == "--abbrev"
                || flag == "--no-abbrev"
                || flag.starts_with("--abbrev=")
                || flag == "--sort=refname"
                || flag == "--no-sort"
                || flag == "--no-delete"
                || flag == "--no-list"
                || flag == "--no-show-current"
                || flag == "--no-format"
                || flag == "--omit-empty"
                || flag == "--no-omit-empty" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [flag] if flag == "--show-current" => {
            if let Some(branch) = store.current_branch()? {
                println!("{branch}");
            }
            Ok(())
        }
        [show_current, no_show_current]
            if show_current == "--show-current" && no_show_current == "--no-show-current" =>
        {
            print_branch_list(store, BranchListMode::Local)
        }
        [delete, no_delete, branch]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(git_dir, format, store, branch, None)
        }
        [delete, no_delete, branch, start]
            if (delete == "-d" || delete == "--delete") && no_delete == "--no-delete" =>
        {
            create_branch_from_start(git_dir, format, store, branch, Some(start))
        }
        [flag] if flag == "-f" || flag == "--force" => print_branch_list(store, BranchListMode::Local),
        [flag, branches @ ..] if flag == "-D" => {
            force_delete_branches(git_dir, format, store, branches, false)
        }
        [flag, force, branches @ ..]
            if (flag == "-d" || flag == "--delete") && (force == "-f" || force == "--force") =>
        {
            force_delete_branches(git_dir, format, store, branches, false)
        }
        [force, flag, branches @ ..]
            if (force == "-f" || force == "--force") && (flag == "-d" || flag == "--delete") =>
        {
            force_delete_branches(git_dir, format, store, branches, false)
        }
        [flag, branches @ ..] if flag == "-d" || flag == "--delete" => {
            delete_merged_branches(git_dir, format, store, branches, false)
        }
        [flag, branch] if flag == "-f" || flag == "--force" => {
            force_update_branch(git_dir, format, store, branch, None).map(|_| ())
        }
        [flag, branch, start] if flag == "-f" || flag == "--force" => {
            force_update_branch(git_dir, format, store, branch, Some(start)).map(|_| ())
        }
        [branch] => {
            create_branch_from_start(git_dir, format, store, branch, None)?;
            branch_create_set_tracking(git_dir, store, branch, None, None, false)
        }
        [branch, start] => {
            create_branch_from_start(git_dir, format, store, branch, Some(start))?;
            branch_create_set_tracking(git_dir, store, branch, Some(start), None, false)
        }
        _ => Err(GitError::Command(
            "branch currently supports: branch [--list [<pattern>...]] [<name> [<start>]] or branch -d|-D <name>... or branch --force <name> [<start>]"
                .into(),
        )),
    }
}
