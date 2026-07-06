//! Table-driven argv helpers for [`super::positional`].
//!
//! Starter for migrating the mechanical `match` arms: consolidate repeated
//! remote/all list permutations here before the full argv table lands post-W90.

use super::list::print_branch_list_remote_or_all_flag;
use crate::*;

/// `-r`/`-a` list argv with one or two trailing noop display flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteOrAllNoopDisplayCount {
    One,
    Two,
}

const REMOTE_OR_ALL_NOOP_DISPLAY_TABLE: &[(RemoteOrAllNoopDisplayCount, bool)] = &[
    (RemoteOrAllNoopDisplayCount::One, false),
    (RemoteOrAllNoopDisplayCount::Two, false),
];

/// Dispatch `-r`/`-a` + noop-display permutations shared across many match arms.
pub(super) fn dispatch_remote_or_all_noop_display(
    store: &FileRefStore,
    flag: &str,
    noop_count: RemoteOrAllNoopDisplayCount,
) -> Result<()> {
    let _ = REMOTE_OR_ALL_NOOP_DISPLAY_TABLE
        .iter()
        .find(|(count, _)| *count == noop_count)
        .expect("table contains all noop-display counts");
    print_branch_list_remote_or_all_flag(store, flag)
}