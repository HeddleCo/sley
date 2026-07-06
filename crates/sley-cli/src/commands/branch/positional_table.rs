//! Table-driven argv helpers for [`super::positional`].
//!
//! Starter for migrating the mechanical `match` arms: consolidate repeated
//! remote/all list, color/column/abbrev noop-flag, and noop-display
//! permutations here before the full argv table lands post-W90.

use super::list::{
    branch_remote_or_all_mode_unchecked, print_branch_list, print_branch_list_matching,
    print_branch_list_remote_or_all_flag, BranchListMode,
};
use crate::*;

/// `-r`/`-a` list argv with one or two trailing noop display flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteOrAllNoopDisplayCount {
    One,
    Two,
}

/// Dispatch `-r`/`-a` + noop-display permutations shared across many match arms.
pub(super) fn dispatch_remote_or_all_noop_display(
    store: &FileRefStore,
    flag: &str,
    _noop_count: RemoteOrAllNoopDisplayCount,
) -> Result<()> {
    // `noop_count` distinguishes one- vs two-flag argv shapes at the match site;
    // both resolve to the same list output.
    print_branch_list_remote_or_all_flag(store, flag)
}

/// `--list` + noop-display argv, optionally with trailing match patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LocalListNoopDisplayTail {
    None,
    Patterns,
}

/// Dispatch local `--list` + noop-display permutations (flag order agnostic).
pub(super) fn dispatch_local_list_noop_display(
    store: &FileRefStore,
    tail: LocalListNoopDisplayTail,
    patterns: &[String],
) -> Result<()> {
    match tail {
        LocalListNoopDisplayTail::None => print_branch_list(store, BranchListMode::Local),
        LocalListNoopDisplayTail::Patterns => {
            print_branch_list_matching(store, BranchListMode::Local, patterns, false)
        }
    }
}

/// `-r`/`-a` + `--list` + noop-display argv, optionally with trailing patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteOrAllListNoopDisplayTail {
    None,
    Patterns,
}

/// Dispatch `-r`/`-a` + `--list` + noop-display permutations (flag order agnostic).
pub(super) fn dispatch_remote_or_all_list_noop_display(
    store: &FileRefStore,
    flag: &str,
    tail: RemoteOrAllListNoopDisplayTail,
    patterns: &[String],
) -> Result<()> {
    let mode = branch_remote_or_all_mode_unchecked(flag);
    match tail {
        RemoteOrAllListNoopDisplayTail::None => print_branch_list(store, mode),
        RemoteOrAllListNoopDisplayTail::Patterns => {
            print_branch_list_matching(store, mode, patterns, false)
        }
    }
}