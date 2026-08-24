use super::*;
use sley::plumbing::{sley_core, sley_diff_merge};
use std::sync::Arc;

// ===== git merge (3-way) =====
//
// Stage-B1 relocation: the canonical implementations of the flattened-tree
// merge adapter, the index/worktree appliers, and the strategy-option plumbing
// live in `sley_sequencer::apply`; the historical `commands::merge_rebase::*`
// paths below are thin shims that inject the CLI's partial-clone hydration.

/// Canonical types shared with the sequencer engines.
pub(crate) use sley_sequencer::apply::{
    MergeConflictPaths, MergeInfoMessages, MergePathBinaryResolver, MergePathFavorResolver,
    MergePathMarkerSizeResolver, MergePathResult, MergePathResults, MergeTreeMap,
    ThreeWayMergeOutcome, merge_index_entry,
    merge_refuse_if_current_working_directory_becomes_file, merge_remove_worktree_file,
    merge_write_worktree_file,
};

/// Host-side partial-clone hydration handed to the sequencer apply backend.
struct MergePrefetch;

impl sley_sequencer::apply::PromisorObjectFetch for MergePrefetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<Arc<EncodedObject>> {
        crate::read_object_maybe_prefetch_promisor(db, oid, true)
    }
}

fn apply_fetch(
    lazy_fetch: bool,
) -> Option<&'static dyn sley_sequencer::apply::PromisorObjectFetch> {
    static PREFETCH: MergePrefetch = MergePrefetch;
    lazy_fetch.then_some(&PREFETCH)
}

pub(crate) fn merge_read_blob(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<Vec<u8>> {
    sley_sequencer::apply::merge_read_blob_with_fetch(db, oid, apply_fetch(lazy_fetch))
}

pub(crate) fn merge_worktree_content(
    db: &FileObjectDatabase,
    mode: u32,
    oid: &ObjectId,
    lazy_fetch: bool,
) -> Result<Vec<u8>> {
    sley_sequencer::apply::merge_worktree_content(db, mode, oid, apply_fetch(lazy_fetch))
}

/// Clear worktree files that block any directory path in the merged result.
/// Used on the clean-merge checkout path before
/// [`sley_worktree::reset_index_and_worktree_to_commit`], which would otherwise
/// fail when a HEAD file occupies a path the merged tree now needs as a
/// directory (directory-rename D/F). Best-effort: errors are swallowed so a
/// genuine I/O problem surfaces from the subsequent checkout instead.
pub(crate) fn clear_merge_df_blockers(worktree_root: &Path, results: &MergePathResults) {
    for (path, result) in results {
        // Deleted paths do not require an ancestor directory. Considering them
        // here can unlink a surviving HEAD file merely because its former
        // child appears as `Resolved(None)` in the flattened merge result.
        if !matches!(result, MergePathResult::Resolved(Some(_))) {
            continue;
        }
        if !path.contains(&b'/') {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(path) {
            let mut prefix = String::new();
            let mut components = rel.split('/').peekable();
            while let Some(component) = components.next() {
                if components.peek().is_none() {
                    break;
                }
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                let candidate = worktree_root.join(&prefix);
                // Best-effort: errors are swallowed so a genuine I/O problem
                // surfaces from the subsequent checkout instead.
                if fs::symlink_metadata(&candidate).is_ok_and(|meta| !meta.is_dir()) {
                    let _ = fs::remove_file(&candidate);
                }
            }
        }
    }
}

/// True when it is safe to delete the worktree file at `path` during a merge:
/// either the file is already gone, or its on-disk content hashes to the blob
/// `ours` (HEAD) had at that path. An untracked file (ours = `None`) or a file
/// whose content diverges from ours' version is preserved, matching git's refusal
/// to clobber untracked/dirty data (the rename/delete "Gollum's ring" case).
pub(crate) fn worktree_file_matches_ours(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    ours: Option<&(u32, ObjectId)>,
) -> Result<bool> {
    let _ = db;
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    let Ok(bytes) = fs::read(&full) else {
        // Missing/unreadable: nothing to clobber, removal is a no-op anyway.
        return Ok(true);
    };
    let Some((_, ours_oid)) = ours else {
        // The path was not tracked on ours' side; on-disk content is untracked.
        return Ok(false);
    };
    let format = ours_oid.format();
    let on_disk = sley_core::object_id_for_bytes(format, "blob", &bytes)?;
    Ok(&on_disk == ours_oid)
}

/// 3-way merge of three flattened trees. Writes any cleanly-merged blob content
/// to the ODB and returns per-path results plus the sorted list of conflicted
/// paths.
///
/// This is a thin adapter over the library seam
/// [`sley_diff_merge::merge_entry_maps`]: the resolution logic lives there, and
/// this function only re-shapes the per-path library result into the
/// index/worktree-oriented [`MergePathResult`] the merge / cherry-pick / revert
/// porcelains consume. It is rename-aware (the merge-ort non-recursive rename
/// case) because the library merge runs with rename detection enabled.
pub(crate) fn three_way_merge_trees(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    three_way_merge_trees_with_favor(
        db,
        config,
        lazy_fetch,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        sley_diff_merge::MergeFavor::None,
    )
}

/// Like [`three_way_merge_trees`] with an explicit diff3 ancestor label and
/// conflict-marker style (cherry-pick / revert pass the parent/commit labels
/// and honour `merge.conflictStyle`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_styled(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    style: sley_diff_merge::ConflictStyle,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    three_way_merge_trees_styled_with_strategy_options(
        db,
        config,
        lazy_fetch,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        style,
        &[],
    )
}

/// Styled three-way merge for sequencer operations, preserving the replayed
/// command's `-X` strategy options instead of dropping them at the CLI seam.
#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_styled_with_strategy_options(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    style: sley_diff_merge::ConflictStyle,
    strategy_options: &[String],
) -> Result<(MergePathResults, MergeConflictPaths)> {
    let (results, conflicts, _) = three_way_merge_trees_inner_with_info_opts(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        merge_favor_from_strategy_opts(strategy_options),
        style,
        merge_ws_ignore_from_strategy_opts(strategy_options),
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_config(config),
            directory_renames: directory_renames_config(config),
            lazy_fetch,
        },
    )?;
    Ok((results, conflicts))
}

/// Delegates to [`sley_diff_merge::virtual_ancestor_entry_map_with_style`].
///
/// `style` is the effective `merge.conflictStyle` so nested virtual-ancestor
/// markers (t6416 nested conflicts) match git's recursive merge.
pub(crate) fn virtual_ancestor_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    bases: &[ObjectId],
    git_dir: &Path,
    style: sley_diff_merge::ConflictStyle,
) -> Result<MergeTreeMap> {
    sley_diff_merge::virtual_ancestor_entry_map_with_style(
        db,
        format,
        bases,
        style,
        |left, right| merge_bases(git_dir, db, format, left, right),
    )
}

/// Resolve `merge.conflictStyle` the way `git merge` does.
pub(crate) fn merge_conflict_style_from_config(
    config: &GitConfig,
) -> sley_diff_merge::ConflictStyle {
    config
        .get("merge", None, "conflictstyle")
        .map(|value| match value {
            "diff3" => sley_diff_merge::ConflictStyle::Diff3,
            "zdiff3" => sley_diff_merge::ConflictStyle::ZDiff3,
            _ => sley_diff_merge::ConflictStyle::Merge,
        })
        .unwrap_or(sley_diff_merge::ConflictStyle::Merge)
}

/// Like [`three_way_merge_trees`] but with an explicit `-Xours`/`-Xtheirs`
/// conflict-favouring choice (used by `git merge -X ours|theirs`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_with_favor(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    favor: sley_diff_merge::MergeFavor,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    three_way_merge_trees_inner(
        db,
        config,
        lazy_fetch,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        "merged common ancestors",
        favor,
        sley_diff_merge::ConflictStyle::Merge,
    )
}

pub(crate) fn merge_favor_from_strategy_opt(value: &str) -> Option<sley_diff_merge::MergeFavor> {
    match value {
        "ours" => Some(sley_diff_merge::MergeFavor::Ours),
        "theirs" => Some(sley_diff_merge::MergeFavor::Theirs),
        _ => None,
    }
}

pub(crate) fn merge_favor_from_strategy_opts(opts: &[String]) -> sley_diff_merge::MergeFavor {
    let mut favor = sley_diff_merge::MergeFavor::None;
    for opt in opts {
        if let Some(next) = merge_favor_from_strategy_opt(opt) {
            favor = next;
        }
    }
    favor
}

pub(crate) fn merge_ws_ignore_from_strategy_opts(opts: &[String]) -> sley_diff_merge::WsIgnore {
    let mut whitespace = sley_diff_merge::WsIgnore::EMPTY;
    for opt in opts {
        match opt.as_str() {
            "ignore-space-change" => whitespace.space_change = true,
            "ignore-all-space" => whitespace.all_space = true,
            "ignore-space-at-eol" => whitespace.space_at_eol = true,
            "ignore-cr-at-eol" => whitespace.cr_at_eol = true,
            _ => {}
        }
    }
    whitespace
}

#[allow(clippy::too_many_arguments)]
fn three_way_merge_trees_inner(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    let (results, conflicts, _) = three_way_merge_trees_inner_with_info(
        db,
        config,
        lazy_fetch,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
    )?;
    Ok((results, conflicts))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_inner_with_info(
    db: &FileObjectDatabase,
    config: &GitConfig,
    lazy_fetch: bool,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
) -> Result<(MergePathResults, MergeConflictPaths, MergeInfoMessages)> {
    three_way_merge_trees_inner_with_info_opts(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
        // Porcelains that don't expose `-Xignore-space-*` use the exact merge.
        sley_diff_merge::WsIgnore::EMPTY,
        // Rename-aware merge with git's default settings: detection on, 50%
        // threshold, `merge.directoryRenames` honoured.
        RenameMergeConfig {
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_config(config),
            directory_renames: directory_renames_config(config),
            lazy_fetch,
        },
    )
}

/// Rename-detection settings threaded into a 3-way merge. `git merge-recursive`
/// (and `git merge -s recursive/ort`) lets the caller tune these via
/// `--find-renames`/`--rename-threshold`/`--no-renames` and the
/// `merge.renames`/`diff.renames` config; the porcelains that don't expose those
/// knobs use [`RenameMergeConfig::default`] (git's defaults).
#[derive(Clone, Copy)]
pub(crate) struct RenameMergeConfig {
    pub(crate) detect_renames: bool,
    pub(crate) rename_threshold: u8,
    pub(crate) rename_limit: usize,
    pub(crate) directory_renames: sley_diff_merge::DirectoryRenames,
    pub(crate) lazy_fetch: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_inner_with_info_opts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
    ws_ignore: sley_diff_merge::WsIgnore,
    renames: RenameMergeConfig,
) -> Result<(MergePathResults, MergeConflictPaths, MergeInfoMessages)> {
    three_way_merge_trees_inner_with_info_opts_and_path_favor(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
        ws_ignore,
        renames,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_inner_with_info_opts_and_path_favor(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
    ws_ignore: sley_diff_merge::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
) -> Result<(MergePathResults, MergeConflictPaths, MergeInfoMessages)> {
    three_way_merge_trees_inner_with_info_opts_and_path_resolvers(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
        ws_ignore,
        renames,
        path_favor,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_inner_with_info_opts_and_path_resolvers(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
    ws_ignore: sley_diff_merge::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
    path_marker_size: Option<&MergePathMarkerSizeResolver<'_>>,
) -> Result<(MergePathResults, MergeConflictPaths, MergeInfoMessages)> {
    let outcome = three_way_merge_trees_outcome_with_info_opts_and_path_resolvers(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
        ws_ignore,
        renames,
        path_favor,
        path_marker_size,
        None,
    )?;
    Ok((outcome.results, outcome.conflicts, outcome.info_messages))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_outcome_with_info_opts_and_path_resolvers(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: sley_diff_merge::MergeFavor,
    style: sley_diff_merge::ConflictStyle,
    ws_ignore: sley_diff_merge::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
    path_marker_size: Option<&MergePathMarkerSizeResolver<'_>>,
    path_is_binary: Option<&MergePathBinaryResolver<'_>>,
) -> Result<ThreeWayMergeOutcome> {
    // Canonical engine in `sley_sequencer::apply`; the CLI shape's
    // `lazy_fetch` flag is injected as the promisor hydration adapter.
    sley_sequencer::apply::three_way_merge_trees_outcome_with_info_opts_and_path_resolvers(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        favor,
        style,
        ws_ignore,
        sley_sequencer::apply::RenameMergeConfig {
            detect_renames: renames.detect_renames,
            rename_threshold: renames.rename_threshold,
            rename_limit: renames.rename_limit,
            directory_renames: renames.directory_renames,
        },
        path_favor,
        path_marker_size,
        path_is_binary,
        apply_fetch(renames.lazy_fetch),
    )
}
