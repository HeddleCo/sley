//! Three-way tree merge engine.

use sley_core::{BString, GitError, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntries, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crate::blob_merge::{
    merge_blobs, ConflictStyle, MergeBlobOptions, MergeBlobResult, MergeFavor,
};
use crate::line_diff::WsIgnore;
use crate::name_status::{
    diff_name_status_maps_with_renames, flatten_tree, is_type_change, DiffNameStatusOptions,
    MergeEntryMap, NameStatus, NameStatusEntry, TrackedEntry, DEFAULT_RENAME_THRESHOLD,
};

// ===========================================================================
// Library tree-merge seam (`merge_trees`).
//
// This is the single 3-way tree-merge engine that every merge porcelain calls.
// Before it existed the logic was duplicated across the CLI: `merge-tree
// --write-tree` had its own copy and `git merge` / `cherry-pick` / `revert`
// had a second copy. Both copies implemented the identical per-path diff3
// resolution; the only differences were *rendering* (write-tree emits a tree +
// stage list + messages; the porcelains stage an index + materialize a
// worktree). This seam computes the merge once and returns a per-path result
// rich enough for both renderings, so the resolution lives in exactly one
// place.
//
// The result is byte-identical to the old per-command copies on every cell
// they already handled (clean merges, content / add-add / modify-delete
// conflicts, mode merges). On top of that it adds rename-aware resolution: a
// file renamed on one side and modified on the other follows the rename,
// gated by [`MergeTreesOptions::detect_renames`] (the classic merge-ort
// non-recursive rename case).
// ===========================================================================

/// Options controlling a [`merge_trees`] run.
pub struct MergeTreesOptions<'a> {
    /// Conflict-marker label for ours (e.g. a branch name or `HEAD`).
    pub ours_label: &'a str,
    /// Conflict-marker label for theirs.
    pub theirs_label: &'a str,
    /// Diff3 ancestor label (the `|||||||` side); merge porcelains use
    /// `"merged common ancestors"`.
    pub ancestor_label: &'a str,
    /// `-Xours` / `-Xtheirs` favouring for textual conflicts.
    pub favor: MergeFavor,
    /// Optional per-path favor, used only when [`Self::favor`] is
    /// [`MergeFavor::None`]. Merge porcelains use this for attributes such as
    /// `merge=union` without changing the command-line `-X` override.
    pub path_favor: Option<&'a dyn Fn(&[u8]) -> MergeFavor>,
    /// Optional per-path conflict marker length resolver for attributes such as
    /// `conflict-marker-size`.
    pub path_marker_size: Option<&'a dyn Fn(&[u8]) -> usize>,
    /// Enable rename-aware merging: a file renamed on one side and modified on
    /// the other follows the rename. When `false`, the merge is purely
    /// path-keyed (the historical behaviour).
    pub detect_renames: bool,
    /// Minimum similarity (`0..=100`) for inexact rename detection.
    pub rename_threshold: u8,
    /// Cap on the inexact rename matrix (`merge.renameLimit`/`diff.renameLimit`).
    /// `0` means unlimited; otherwise inexact detection is skipped when the
    /// candidate source × destination count exceeds `rename_limit²`.
    pub rename_limit: usize,
    /// Directory-rename detection mode. When [`DirectoryRenames::False`], a file
    /// added on one side under a directory that the *other* side renamed stays
    /// put. When enabled, such files are re-homed into the renamed directory,
    /// matching `merge.directoryRenames`. Requires `detect_renames` to have any
    /// effect (directory renames are inferred from the file renames it finds).
    pub directory_renames: DirectoryRenames,
    /// Conflict-marker style for textual conflicts (`merge.conflictStyle`).
    pub style: ConflictStyle,
    /// Whitespace-insensitivity for textual 3-way merges, mirroring
    /// `-Xignore-space-change`/`-Xignore-all-space`/`-Xignore-space-at-eol`.
    pub ws_ignore: WsIgnore,
}

/// How directory-rename detection behaves, mirroring git's
/// `merge.directoryRenames` configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DirectoryRenames {
    /// Disable directory-rename detection (`merge.directoryRenames=false`).
    #[default]
    False,
    /// Apply directory renames silently (`merge.directoryRenames=true`).
    True,
    /// Detect directory renames but treat each re-homed path as a conflict
    /// requiring confirmation (`merge.directoryRenames=conflict`). git's default.
    Conflict,
}

impl Default for MergeTreesOptions<'_> {
    fn default() -> Self {
        Self {
            ours_label: "ours",
            theirs_label: "theirs",
            ancestor_label: "merged common ancestors",
            favor: MergeFavor::None,
            path_favor: None,
            path_marker_size: None,
            detect_renames: false,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
            directory_renames: DirectoryRenames::False,
            style: ConflictStyle::Merge,
            ws_ignore: WsIgnore::EMPTY,
        }
    }
}

fn merge_favor_for_path(options: &MergeTreesOptions<'_>, path: &[u8]) -> MergeFavor {
    if options.favor != MergeFavor::None {
        return options.favor;
    }
    options
        .path_favor
        .map(|resolver| resolver(path))
        .unwrap_or(MergeFavor::None)
}

fn merge_marker_size_for_path(options: &MergeTreesOptions<'_>, path: &[u8]) -> usize {
    options
        .path_marker_size
        .map(|resolver| resolver(path))
        .unwrap_or(7)
}

/// The kind of conflict recorded for a path, used to render the stable
/// conflict-type token and human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeConflictKind {
    /// Both sides changed the file content differently (or both added it with
    /// differing content — an add/add).
    Content { add_add: bool },
    /// The file was deleted on one side and modified on the other.
    ModifyDelete {
        /// The side label that deleted the path.
        deleted_in: String,
        /// The side label that modified (and thus kept) the path.
        modified_in: String,
    },
    /// A file renamed on one side, with a content conflict against the other
    /// side's change at the destination.
    RenameContent {
        /// The original (pre-rename) path.
        old_path: Vec<u8>,
    },
    /// Two paths were renamed to the same destination, producing a
    /// rename/rename(2to1) conflict.
    RenameRenameTwoToOne {
        /// Ours' pre-destination path.
        ours_path: Vec<u8>,
        /// Theirs' pre-destination path.
        theirs_path: Vec<u8>,
    },
    /// One source path was renamed to different destinations on each side,
    /// producing a rename/rename(1to2) conflict.
    RenameRenameOneToTwo {
        /// The pre-rename source path.
        old_path: Vec<u8>,
        /// Ours' destination path.
        ours_path: Vec<u8>,
        /// Theirs' destination path.
        theirs_path: Vec<u8>,
        /// The label for our side.
        ours_label: String,
        /// The label for their side.
        theirs_label: String,
    },
    /// An auxiliary higher-stage entry for a rename/rename(1to2) conflict. The
    /// user-facing message is emitted by [`RenameRenameOneToTwo`].
    RenameRenameOneToTwoStage,
    /// A directory was split evenly across multiple destinations, so no
    /// directory rename could be applied for paths the other side left there.
    DirRenameSplit {
        /// The original directory with no unique destination.
        source_dir: Vec<u8>,
    },
    /// A file renamed on one side whose source was deleted on the other side.
    RenameDelete {
        /// The pre-rename source path.
        old_path: Vec<u8>,
        /// The side label that performed the rename.
        renamed_in: String,
        /// The side label that deleted the source.
        deleted_in: String,
    },
    /// A file collides with a directory at the same path in the merged result:
    /// the directory wins at the original path and the file is moved aside to
    /// `path~<branch>` (merge-ort's D/F conflict, `unique_path`). git emits
    /// `CONFLICT (file/directory): directory in the way of <old> from <branch>;
    /// moving it to <new> instead.`
    FileDirectory {
        /// The original (pre-move) path now occupied by the directory.
        original_path: Vec<u8>,
        /// The side label whose file was moved aside.
        moved_from: String,
    },
    /// A path was added/renamed under a directory the other side renamed, so the
    /// merge silently moved it into the renamed directory but, in
    /// `merge.directoryRenames=conflict` mode, flags it for the user to confirm.
    /// git emits `CONFLICT (file location): ... suggesting it should perhaps be
    /// moved to <new_path>.` The tree still contains the re-homed content.
    DirRenameLocation {
        /// The pre-re-home path (`old_path` in git's message): where the side
        /// placed the file before directory-rename detection moved it.
        old_path: Vec<u8>,
        /// `Some(source)` when the file was *renamed* into `old_path` by this
        /// side (git's "renamed to" wording, naming the original `source`);
        /// `None` when it was a fresh add (git's "added in" wording).
        renamed_from: Option<Vec<u8>>,
        /// The side label that added/renamed the file (`branch_with_new_path`).
        added_in: String,
        /// The side label that renamed the directory (`branch_with_dir_rename`).
        dir_renamed_in: String,
        /// True when the directory rename moved the file back onto its own base
        /// source path (rename-to-self) and the other side modified that path. The
        /// `CONFLICT (file location)` message is the same, but git records the
        /// path UNMERGED (stages 1/2/3) instead of staging the re-homed content
        /// cleanly: the index writers stage these 1/2/3, not at stage 0.
        back_to_self: bool,
    },
    /// A directory rename would have moved one or more paths onto this path, but
    /// it is already occupied (a file/dir in the way) or several sources map
    /// here. git emits `CONFLICT (implicit dir rename): Existing file/dir at
    /// <path> in the way of implicit directory rename(s) putting the following
    /// path(s) there: <sources>.` The path keeps its original content; the
    /// re-homed sources are left where they were.
    DirRenameImplicitCollision {
        /// The source path(s) the directory rename tried to move onto this path.
        sources: Vec<Vec<u8>>,
    },
    /// The two sides hold different object types at one path (regular↔symlink,
    /// regular↔gitlink, symlink↔gitlink). git's `process_entry` (merge-ort.c
    /// ~4220) renames each *regular-file* side to `path~<branch>` so each type
    /// can be recorded somewhere, ignoring `-Xours`/`-Xtheirs`, and emits a
    /// single `CONFLICT (distinct types)` line. (gitlink↔gitlink and
    /// symlink↔symlink share an `S_IFMT` and never reach this arm.) This kind is
    /// attached to the leaf that carries the message — the side left at
    /// `original_path` when only one side moved, else ours; the other renamed
    /// leaf carries [`DistinctTypesStage`].
    DistinctTypes {
        /// The original colliding path (git's message subject and sort key).
        original_path: Vec<u8>,
        /// `Some(p)` when ours was renamed aside to `p`; `None` when ours stayed
        /// at `original_path`.
        ours_renamed: Option<Vec<u8>>,
        /// `Some(p)` when theirs was renamed aside to `p`; `None` when theirs
        /// stayed at `original_path`.
        theirs_renamed: Option<Vec<u8>>,
    },
    /// The non-message-carrying leaf of a [`DistinctTypes`] conflict. The
    /// user-facing line is emitted once by the [`DistinctTypes`] leaf.
    DistinctTypesStage,
}

/// One resolved/conflicted path in the merged tree.
#[derive(Debug, Clone)]
pub struct MergedPath {
    /// Destination path in the merged tree.
    pub path: Vec<u8>,
    /// The per-stage (1=base, 2=ours, 3=theirs) entries when conflicted; all
    /// `None` for a clean resolution.
    pub stages: MergeStages,
    /// `Some((mode, oid))` is the final leaf written to the merged tree; `None`
    /// means the path is absent in the result (a clean delete).
    pub result: Option<(u32, ObjectId)>,
    /// When conflicted, the worktree bytes + mode to materialize (content with
    /// conflict markers, or the surviving side's bytes). `None` for a clean
    /// path.
    pub worktree: Option<(u32, Vec<u8>)>,
    /// `Some(..)` exactly when this path conflicted.
    pub conflict: Option<MergeConflictKind>,
    /// True when this path went through a textual 3-way content merge (both
    /// sides diverged and both were mergeable files). Drives the "Auto-merging
    /// <path>" informational message, which `git merge-tree` emits for every
    /// such path — clean or conflicted.
    pub auto_merged: bool,
}

impl MergedPath {
    /// True when this path resolved cleanly (no conflict recorded).
    pub fn is_clean(&self) -> bool {
        self.conflict.is_none()
    }
}

/// Per-stage higher-order index entries for a conflicted path.
#[derive(Debug, Clone, Default)]
pub struct MergeStages {
    pub base: Option<(u32, ObjectId)>,
    pub ours: Option<(u32, ObjectId)>,
    pub theirs: Option<(u32, ObjectId)>,
}

/// The outcome of a 3-way tree merge: the merged top-level tree plus per-path
/// detail and a clean/conflicted flag.
#[derive(Debug, Clone)]
pub struct MergeTreesResult {
    /// Object id of the merged top-level tree (always written, even on
    /// conflict — conflicted blobs go in with their marker content).
    pub tree: ObjectId,
    /// Per-path results, sorted by path.
    pub paths: Vec<MergedPath>,
    /// False if any path conflicted.
    pub clean: bool,
    /// Original paths removed by rename or directory-rename rewrites. These are
    /// cleanup-only paths for porcelains materializing a conflicted merge; they
    /// are absent from the merged tree.
    pub cleanup_paths: Vec<Vec<u8>>,
    /// Non-conflict informational messages produced while detecting renames.
    pub info_messages: Vec<MergeInfoMessage>,
}

impl MergeTreesResult {
    /// Iterate over the paths that conflicted, in path order.
    pub fn conflicts(&self) -> impl Iterator<Item = &MergedPath> {
        self.paths.iter().filter(|entry| entry.conflict.is_some())
    }
}

/// Non-conflict merge information that porcelain commands may print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeInfoMessage {
    /// A directory rename was skipped because the suggested target directory was
    /// itself renamed away on this side.
    DirRenameSkippedDueToRerename {
        old_dir: Vec<u8>,
        path: Vec<u8>,
        new_dir: Vec<u8>,
    },
    /// A path was updated due to a directory rename in
    /// `merge.directoryRenames=true` mode.
    DirRenameApplied {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_from: Option<Vec<u8>>,
        added_in: String,
        dir_renamed_in: String,
    },
    /// A directory-rename location conflict that overlaps another conflict at
    /// the same final path, such as a content conflict. The path's primary
    /// conflict kind remains attached to the path; this carries git's extra
    /// `CONFLICT (file location)` line.
    DirRenameLocationConflict {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_from: Option<Vec<u8>>,
        added_in: String,
        dir_renamed_in: String,
    },
    /// A rename/delete conflict whose conflicted destination was later moved
    /// aside by directory/file conflict handling. The primary per-path conflict
    /// remains `FileDirectory`; this preserves git's extra rename/delete line.
    RenameDeleteConflict {
        old_path: Vec<u8>,
        new_path: Vec<u8>,
        renamed_in: String,
        deleted_in: String,
    },
}


/// True for a plain file blob (regular or executable) — i.e. a mode whose
/// content can be textually 3-way merged. Symlinks and gitlinks are excluded.
pub fn is_mergeable_file_mode(mode: u32) -> bool {
    mode == 0o100644 || mode == 0o100755
}

/// 3-way merge of three trees into a single merged tree.
///
/// `base` is the common-ancestor tree (`None` for unrelated histories — every
/// path is then treated as added on both sides). `ours`/`theirs` are the two
/// sides. Cleanly-merged blob content and the resulting (sub)trees are written
/// to `db`; the returned [`MergeTreesResult`] carries the merged top-level tree
/// oid plus per-path detail.
///
/// This is the shared engine behind `git merge-tree --write-tree`, `git merge`,
/// `git cherry-pick`, and `git revert`. It is behaviour-preserving relative to
/// the per-command copies it replaced, and additionally resolves renames when
/// [`MergeTreesOptions::detect_renames`] is set.
pub fn merge_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: Option<&ObjectId>,
    ours: &ObjectId,
    theirs: &ObjectId,
    options: &MergeTreesOptions<'_>,
) -> Result<MergeTreesResult> {
    let base_map = match base {
        Some(tree) => flatten_tree(db, format, tree)?,
        None => MergeEntryMap::new(),
    };
    let ours_map = flatten_tree(db, format, ours)?;
    let theirs_map = flatten_tree(db, format, theirs)?;
    merge_entry_maps(db, format, &base_map, &ours_map, &theirs_map, options)
}

/// [`merge_trees`] operating on already-flattened entry maps. The merge
/// porcelains often hold the flattened maps already (e.g. cherry-pick builds
/// `theirs` from a picked commit's tree), so this avoids re-reading them.
pub fn merge_entry_maps(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<MergeTreesResult> {
    // Rename-aware step: detect files renamed on exactly one side relative to
    // base, so a modification on the other side follows the rename. This is the
    // non-recursive merge-ort rename case. We compute a rewrite map that, for a
    // one-sided rename old->new, presents the *other* side's `old` content at
    // `new` (and drops `old`), letting the path-keyed core below do the 3-way
    // content merge at the destination.
    let (mut renames, side_renames) = if options.detect_renames {
        let (renames, ours_side, theirs_side) =
            detect_merge_renames(db, format, base_map, ours_map, theirs_map, options)?;
        (renames, Some((ours_side, theirs_side)))
    } else {
        (MergeRenames::default(), None)
    };

    // Build the effective per-side maps with file renames applied.
    let (mut eff_base, mut eff_ours, mut eff_theirs) =
        apply_merge_renames(base_map, ours_map, theirs_map, &renames);

    // Directory-rename detection: when one side renamed a whole directory and
    // the other side added a file under (or renamed a file into) the old
    // directory, re-home that path into the renamed directory — including
    // transitive renames (a file the other side renamed into a directory this
    // side renamed follows on into the final directory). This is the
    // merge.directoryRenames behaviour, applied as a rewrite of the rename/add
    // destination paths so every merged path consults directory renames.
    let mut dir_rename_dirty = false;
    let mut rehomed_paths: BTreeMap<Vec<u8>, RehomeSides> = BTreeMap::new();
    let mut dir_rename_two_to_one: Vec<DirRenameTwoToOne> = Vec::new();
    let mut dir_rename_collisions: Vec<DirRenameCollision> = Vec::new();
    let mut dir_rename_splits: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut dir_rename_back_to_self: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut info_messages = Vec::new();
    let mut cleanup_paths: BTreeSet<Vec<u8>> = renames
        .dest_to_source
        .values()
        .map(|rename| rename.source.clone())
        .collect();
    if options.directory_renames != DirectoryRenames::False
        && let Some((ours_side, theirs_side)) = &side_renames
    {
        let dir_renames = compute_directory_renames(ours_map, theirs_map, ours_side, theirs_side);
        let outcome = apply_directory_renames(
            base_map,
            &eff_base,
            &eff_ours,
            &eff_theirs,
            ours_side,
            theirs_side,
            &dir_renames,
            &renames.dest_to_source,
        );
        eff_base = outcome.base;
        eff_ours = outcome.ours;
        eff_theirs = outcome.theirs;
        rehomed_paths = outcome.rehomed;
        dir_rename_collisions = outcome.collisions;
        dir_rename_splits = outcome.splits;
        dir_rename_back_to_self = outcome.back_to_self;
        info_messages = outcome.info_messages;
        dir_rename_dirty = outcome.dirty;
        remap_rename_destinations(&mut renames, &rehomed_paths);
        drop_collapsed_rename_rename_conflicts(&mut renames);
        dir_rename_two_to_one = collect_dir_rename_two_to_one(&renames, &rehomed_paths);
    }
    for info in rehomed_paths
        .values()
        .flat_map(|sides| [&sides.ours, &sides.theirs])
        .flatten()
    {
        cleanup_paths.insert(info.old_path.clone());
    }
    if options.directory_renames == DirectoryRenames::True {
        for (dest, sides) in &rehomed_paths {
            for info in [&sides.ours, &sides.theirs].into_iter().flatten() {
                let (added_in, dir_renamed_in) = if info.added_on_ours {
                    (
                        options.ours_label.to_string(),
                        options.theirs_label.to_string(),
                    )
                } else {
                    (
                        options.theirs_label.to_string(),
                        options.ours_label.to_string(),
                    )
                };
                info_messages.push(MergeInfoMessage::DirRenameApplied {
                    old_path: info.old_path.clone(),
                    new_path: dest.clone(),
                    renamed_from: info.renamed_from.clone(),
                    added_in,
                    dir_renamed_in,
                });
            }
        }
    }
    // In =conflict mode, every re-homed path is reported as a location conflict
    // (the tree still gets the re-homed content, but the merge is marked dirty).
    let dir_rename_conflict_paths: BTreeMap<Vec<u8>, RehomeSides> =
        if options.directory_renames == DirectoryRenames::Conflict {
            rehomed_paths.clone()
        } else {
            BTreeMap::new()
        };

    let mut all_paths = BTreeSet::new();
    all_paths.extend(eff_base.keys().cloned());
    all_paths.extend(eff_ours.keys().cloned());
    all_paths.extend(eff_theirs.keys().cloned());

    let mut paths: Vec<MergedPath> = Vec::new();
    let mut leaves: MergeEntryMap = BTreeMap::new();
    let mut clean = true;

    for path in all_paths {
        let base = eff_base.get(&path).cloned();
        let ours = eff_ours.get(&path).cloned();
        let theirs = eff_theirs.get(&path).cloned();
        let rename = renames.dest_to_source.get(&path);
        let old_path = rename.map(|r| r.source.clone());
        let favor = merge_favor_for_path(options, &path);

        // Trivial resolutions (identical to the historical per-command logic).
        if ours == theirs {
            if let Some(entry) = ours {
                leaves.insert(path.clone(), entry);
            }
            paths.push(clean_path(path, ours));
            continue;
        }
        if ours == base {
            if let Some(entry) = &theirs {
                leaves.insert(path.clone(), *entry);
            }
            paths.push(clean_path(path, theirs));
            continue;
        }
        if theirs == base {
            if let Some(entry) = &ours {
                leaves.insert(path.clone(), *entry);
            }
            paths.push(clean_path(path, ours));
            continue;
        }

        // Both sides diverged. Decide how to combine.
        let content_mergeable = matches!(&ours, Some((mode, _)) if is_mergeable_file_mode(*mode))
            && matches!(&theirs, Some((mode, _)) if is_mergeable_file_mode(*mode))
            && match &base {
                Some((mode, _)) => is_mergeable_file_mode(*mode),
                None => true,
            };

        if let (true, Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (content_mergeable, &ours, &theirs)
        {
            let add_add = base.is_none();
            let base_bytes = match &base {
                Some((_, oid)) => merge_blob_bytes(db, oid)?,
                None => Vec::new(),
            };
            let ours_bytes = merge_blob_bytes(db, ours_oid)?;
            let theirs_bytes = merge_blob_bytes(db, theirs_oid)?;
            // When this destination came from a one-sided rename, git qualifies
            // the conflict-marker labels with the per-side path (the renaming
            // side shows the new path, the other side the old path), e.g.
            // `<<<<<<< HEAD:old.txt` / `>>>>>>> feature:new.txt`.
            let rehome = rehomed_paths.get(&path);
            // git's `merge_3way` qualifies all three labels with their per-side
            // path (`<name>:<path>`) whenever the three paths are not identical —
            // pathnames[0] is the base/ancestor path (the rename source). When
            // they are identical (no rename), it uses the bare names.
            let (base_label_owned, ours_label, theirs_label) = match rename {
                Some(MergeRename { source, side }) => {
                    let (ours_path, theirs_path) = match side {
                        // theirs renamed -> ours kept the source path.
                        RenameSide::Theirs => (source.as_slice(), path.as_slice()),
                        // ours renamed -> theirs kept the source path.
                        RenameSide::Ours => (path.as_slice(), source.as_slice()),
                    };
                    (
                        qualify_label(options.ancestor_label, source.as_slice()),
                        qualify_label(options.ours_label, ours_path),
                        qualify_label(options.theirs_label, theirs_path),
                    )
                }
                None => {
                    let ours_path = rehome
                        .and_then(|info| info.ours.as_ref())
                        .map_or(path.as_slice(), |info| info.old_path.as_slice());
                    let theirs_path = rehome
                        .and_then(|info| info.theirs.as_ref())
                        .map_or(path.as_slice(), |info| info.old_path.as_slice());
                    if ours_path != path.as_slice() || theirs_path != path.as_slice() {
                        (
                            qualify_label(options.ancestor_label, path.as_slice()),
                            qualify_label(options.ours_label, ours_path),
                            qualify_label(options.theirs_label, theirs_path),
                        )
                    } else {
                        (
                            options.ancestor_label.to_string(),
                            options.ours_label.to_string(),
                            options.theirs_label.to_string(),
                        )
                    }
                }
            };
            let result = merge_blobs(
                &base_bytes,
                &ours_bytes,
                &theirs_bytes,
                &MergeBlobOptions {
                    ours_label: &ours_label,
                    theirs_label: &theirs_label,
                    base_label: &base_label_owned,
                    style: options.style,
                    favor,
                    ws_ignore: options.ws_ignore,
                    marker_size: merge_marker_size_for_path(options, &path),
                },
            );

            let base_mode = base.as_ref().map(|(mode, _)| *mode);
            let (resolved_mode, mode_conflict) =
                merge_file_modes(base_mode, *ours_mode, *theirs_mode);

            if !result.conflicted && !mode_conflict {
                let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content))?;
                leaves.insert(path.clone(), (resolved_mode, oid));
                paths.push(clean_path_auto(path, Some((resolved_mode, oid)), true));
            } else if favor != MergeFavor::None && !mode_conflict {
                let chosen = if favor == MergeFavor::Ours {
                    ours
                } else {
                    theirs
                };
                if let Some(entry) = chosen {
                    leaves.insert(path.clone(), entry);
                }
                paths.push(clean_path_auto(path, chosen, true));
            } else {
                clean = false;
                let oid =
                    db.write_object(EncodedObject::new(ObjectType::Blob, result.content.clone()))?;
                leaves.insert(path.clone(), (resolved_mode, oid));
                let worktree_mode = if *ours_mode == *theirs_mode {
                    *ours_mode
                } else {
                    0o100644
                };
                let conflict = if let Some(old) = &old_path {
                    MergeConflictKind::RenameContent {
                        old_path: old.clone(),
                    }
                } else if add_add {
                    match rehome.and_then(|info| Some((info.ours.as_ref()?, info.theirs.as_ref()?)))
                    {
                        Some((ours_info, theirs_info)) => MergeConflictKind::RenameRenameTwoToOne {
                            ours_path: ours_info.old_path.clone(),
                            theirs_path: theirs_info.old_path.clone(),
                        },
                        None => MergeConflictKind::Content { add_add },
                    }
                } else {
                    MergeConflictKind::Content { add_add }
                };
                paths.push(MergedPath {
                    path: path.clone(),
                    stages: stages_for(&base, &ours, &theirs),
                    result: Some((resolved_mode, oid)),
                    worktree: Some((worktree_mode, result.content)),
                    conflict: Some(conflict),
                    auto_merged: true,
                });
            }
        } else if base.is_some() && (ours.is_none() || theirs.is_none()) {
            // modify/delete.
            clean = false;
            let (deleted_in, modified_in, surviving) = if ours.is_none() {
                (
                    options.ours_label.to_string(),
                    options.theirs_label.to_string(),
                    theirs,
                )
            } else {
                (
                    options.theirs_label.to_string(),
                    options.ours_label.to_string(),
                    ours,
                )
            };
            let worktree = match &surviving {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            if let Some(entry) = surviving {
                leaves.insert(path.clone(), entry);
            }
            paths.push(MergedPath {
                path: path.clone(),
                stages: stages_for(&base, &ours, &theirs),
                result: surviving,
                worktree,
                conflict: Some(MergeConflictKind::ModifyDelete {
                    deleted_in,
                    modified_in,
                }),
                auto_merged: false,
            });
        } else if let (Some(&(ours_mode, ours_oid)), Some(&(theirs_mode, theirs_oid))) =
            (ours.as_ref(), theirs.as_ref())
            && sley_index::is_symlink_mode(ours_mode)
            && sley_index::is_symlink_mode(theirs_mode)
        {
            // Both sides are symlinks that diverged from the base and from each
            // other (the trivial oid resolutions above already took the agreeing
            // cases). A symlink is never textually merged; git's
            // `handle_content_merge` symlink arm (merge-ort.c) resolves CLEAN to
            // a side under `-Xours`/`-Xtheirs`, and otherwise records a CONFLICT
            // carrying ours' target.
            match favor {
                MergeFavor::Ours => {
                    leaves.insert(path.clone(), (ours_mode, ours_oid));
                    paths.push(clean_path_auto(
                        path.clone(),
                        Some((ours_mode, ours_oid)),
                        false,
                    ));
                }
                MergeFavor::Theirs => {
                    leaves.insert(path.clone(), (theirs_mode, theirs_oid));
                    paths.push(clean_path_auto(
                        path.clone(),
                        Some((theirs_mode, theirs_oid)),
                        false,
                    ));
                }
                MergeFavor::None | MergeFavor::Union => {
                    clean = false;
                    leaves.insert(path.clone(), (ours_mode, ours_oid));
                    let worktree =
                        Some((ours_mode, merge_worktree_bytes(db, ours_mode, &ours_oid)?));
                    paths.push(MergedPath {
                        path: path.clone(),
                        stages: stages_for(&base, &ours, &theirs),
                        result: Some((ours_mode, ours_oid)),
                        worktree,
                        conflict: Some(MergeConflictKind::Content {
                            add_add: base.is_none(),
                        }),
                        auto_merged: false,
                    });
                }
            }
        } else if let (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (ours, theirs)
            && is_type_change(ours_mode, theirs_mode)
        {
            // Distinct types at one path: both sides present with different
            // `S_IFMT` (regular↔symlink, regular↔gitlink, symlink↔gitlink).
            // Mirror merge-ort's `process_entry`: rename each regular-file side
            // to `path~<branch>` so each type is recorded somewhere; ignore
            // `-Xours`/`-Xtheirs`. gitlink↔gitlink and symlink↔symlink share an
            // `S_IFMT` and are handled by the arms above.
            clean = false;
            // git renames the regular-file side(s): only the regular side when
            // exactly one is regular, both when neither is (symlink↔gitlink).
            let (rename_ours, rename_theirs) = if is_mergeable_file_mode(ours_mode) {
                (true, false)
            } else if is_mergeable_file_mode(theirs_mode) {
                (false, true)
            } else {
                (true, true)
            };
            // git keeps the base stage (index stage 1) for a side only when that
            // side shares the base's file type.
            let ours_base = base.filter(|(mode, _)| !is_type_change(*mode, ours_mode));
            let theirs_base = base.filter(|(mode, _)| !is_type_change(*mode, theirs_mode));
            // Name and reserve ours' aside-path first so the two renamed paths
            // can never collide (`unique_df_path` consults `leaves`/`paths`).
            let ours_path = if rename_ours {
                unique_df_path(&path, options.ours_label, &leaves, &paths)
            } else {
                path.clone()
            };
            leaves.insert(ours_path.clone(), (ours_mode, ours_oid));
            let theirs_path = if rename_theirs {
                unique_df_path(&path, options.theirs_label, &leaves, &paths)
            } else {
                path.clone()
            };
            leaves.insert(theirs_path.clone(), (theirs_mode, theirs_oid));

            // The message is emitted once, by the leaf left at `original_path`
            // when only one side moved (matching git's keying), else by ours.
            let ours_carries_message = !rename_ours || rename_theirs;
            let distinct = MergeConflictKind::DistinctTypes {
                original_path: path.clone(),
                ours_renamed: rename_ours.then(|| ours_path.clone()),
                theirs_renamed: rename_theirs.then(|| theirs_path.clone()),
            };
            let ours_worktree = Some((ours_mode, merge_worktree_bytes(db, ours_mode, &ours_oid)?));
            paths.push(MergedPath {
                path: ours_path,
                stages: MergeStages {
                    base: ours_base,
                    ours: Some((ours_mode, ours_oid)),
                    theirs: None,
                },
                result: Some((ours_mode, ours_oid)),
                worktree: ours_worktree,
                conflict: Some(if ours_carries_message {
                    distinct.clone()
                } else {
                    MergeConflictKind::DistinctTypesStage
                }),
                auto_merged: false,
            });
            let theirs_worktree = Some((
                theirs_mode,
                merge_worktree_bytes(db, theirs_mode, &theirs_oid)?,
            ));
            paths.push(MergedPath {
                path: theirs_path,
                stages: MergeStages {
                    base: theirs_base,
                    ours: None,
                    theirs: Some((theirs_mode, theirs_oid)),
                },
                result: Some((theirs_mode, theirs_oid)),
                worktree: theirs_worktree,
                conflict: Some(if ours_carries_message {
                    MergeConflictKind::DistinctTypesStage
                } else {
                    distinct
                }),
                auto_merged: false,
            });
        } else {
            // add/add of non-files, mode changes on same-type entries, etc. Keep
            // the surviving side's content and record a generic content conflict.
            clean = false;
            let add_add = base.is_none();
            let surviving = ours.or(theirs);
            let worktree = match &surviving {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            if let Some(entry) = surviving {
                leaves.insert(path.clone(), entry);
            }
            paths.push(MergedPath {
                path: path.clone(),
                stages: stages_for(&base, &ours, &theirs),
                result: surviving,
                worktree,
                conflict: Some(MergeConflictKind::Content { add_add }),
                auto_merged: false,
            });
        }
    }

    if !renames.rename_rename_one_to_two.is_empty() {
        apply_rename_rename_one_to_two_conflicts(
            db,
            base_map,
            &eff_ours,
            &eff_theirs,
            &renames.rename_rename_one_to_two,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    if !dir_rename_two_to_one.is_empty() {
        apply_dir_rename_two_to_one_conflicts(
            db,
            &eff_ours,
            &eff_theirs,
            &dir_rename_two_to_one,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    // Rename/rename(2to1) and rename/add: two distinct contents collide on one
    // destination (and the rename source(s) are consumed). Detected from the full
    // per-side rename sets, applied here so the destination carries both sides'
    // content-merged stages instead of the path-keyed core's raw add/add.
    if !renames.rename_rename_two_to_one.is_empty() || !renames.rename_adds.is_empty() {
        apply_rename_two_to_one_and_add_conflicts(
            db,
            base_map,
            ours_map,
            theirs_map,
            &renames,
            &mut paths,
            &mut leaves,
            options,
        )?;
        clean = false;
    }

    // Rename/delete conflicts: a file renamed on one side whose source the other
    // side deleted. The merge core resolved the destination cleanly (only the
    // renaming side has it), but git flags this as a conflict — keep the renamed
    // content in the tree, record higher-order stages, and mark the merge dirty.
    if !renames.rename_deletes.is_empty() {
        for (dest, rd) in &renames.rename_deletes {
            // Skip if another conflict already claimed this destination.
            let Some(slot) = paths.iter_mut().find(|p| &p.path == dest) else {
                continue;
            };
            if slot.conflict.is_some() {
                continue;
            }
            let base_entry = base_map.get(&rd.source).copied();
            let renamed_entry = slot.result;
            // The renamed content sits on the renaming side; the deleting side
            // contributes no stage at the destination.
            let (ours_stage, theirs_stage) = match rd.side {
                RenameSide::Ours => (renamed_entry, None),
                RenameSide::Theirs => (None, renamed_entry),
            };
            let (renamed_in, deleted_in) = match rd.side {
                RenameSide::Ours => (
                    options.ours_label.to_string(),
                    options.theirs_label.to_string(),
                ),
                RenameSide::Theirs => (
                    options.theirs_label.to_string(),
                    options.ours_label.to_string(),
                ),
            };
            let worktree = match &renamed_entry {
                Some((mode, oid)) => Some((*mode, merge_worktree_bytes(db, *mode, oid)?)),
                None => None,
            };
            slot.stages = MergeStages {
                base: base_entry,
                ours: ours_stage,
                theirs: theirs_stage,
            };
            slot.worktree = worktree;
            slot.conflict = Some(MergeConflictKind::RenameDelete {
                old_path: rd.source.clone(),
                renamed_in,
                deleted_in,
            });
            clean = false;
        }
    }

    // Directory-rename outcomes that make the merge dirty. A collision/split
    // detected while re-homing (two paths onto one destination, an ambiguous
    // split source, or a file in the way) marks the merge unclean regardless of
    // mode. In =conflict mode, every silently re-homed path is *also* reported
    // as a location conflict: the tree keeps the re-homed content but git wants
    // the user to confirm the suggested move.
    if dir_rename_dirty {
        clean = false;
    }
    // Implicit-directory-rename collisions (a directory rename would put a path
    // onto an existing file/dir, or N paths onto one destination). git emits
    // `CONFLICT (implicit dir rename): Existing file/dir at <dest> in the way ...`
    // regardless of mode, and the merge is unclean. Attach the conflict to the
    // blocked destination path (which keeps its original content).
    for collision in &dir_rename_collisions {
        clean = false;
        if let Some(slot) = paths.iter_mut().find(|p| p.path == collision.dest)
            && slot.conflict.is_none()
        {
            slot.conflict = Some(MergeConflictKind::DirRenameImplicitCollision {
                sources: collision.sources.clone(),
            });
        } else if !paths.iter().any(|p| p.path == collision.dest) {
            paths.push(MergedPath {
                path: collision.dest.clone(),
                stages: MergeStages::default(),
                result: None,
                worktree: None,
                conflict: Some(MergeConflictKind::DirRenameImplicitCollision {
                    sources: collision.sources.clone(),
                }),
                auto_merged: false,
            });
        }
    }
    for source_dir in &dir_rename_splits {
        clean = false;
        paths.push(MergedPath {
            path: source_dir.clone(),
            stages: MergeStages::default(),
            result: None,
            worktree: None,
            conflict: Some(MergeConflictKind::DirRenameSplit {
                source_dir: source_dir.clone(),
            }),
            auto_merged: false,
        });
    }
    if !dir_rename_conflict_paths.is_empty() {
        clean = false;
        for (dest, infos) in &dir_rename_conflict_paths {
            for info in [&infos.ours, &infos.theirs].into_iter().flatten() {
                let (added_in, dir_renamed_in) = if info.added_on_ours {
                    // The path was added/renamed by ours, into a dir theirs renamed.
                    (
                        options.ours_label.to_string(),
                        options.theirs_label.to_string(),
                    )
                } else {
                    (
                        options.theirs_label.to_string(),
                        options.ours_label.to_string(),
                    )
                };
                // Rename-to-self via a directory rename (merge-ort 12i2): the
                // re-home landed the file back on its own base source path where
                // the other side modified it. git records this UNMERGED (UU) even
                // though the trivial 3-way at the destination resolves cleanly
                // (the renamed side's content equals base). Stage the three
                // versions so the index carries the conflict.
                let back_to_self = dir_rename_back_to_self.contains(dest);
                if let Some(slot) = paths.iter_mut().find(|p| &p.path == dest)
                    && slot.conflict.is_none()
                {
                    if back_to_self {
                        slot.stages = MergeStages {
                            base: eff_base.get(dest).copied(),
                            ours: eff_ours.get(dest).copied(),
                            theirs: eff_theirs.get(dest).copied(),
                        };
                        slot.worktree = match &slot.result {
                            Some((mode, oid)) => {
                                Some((*mode, merge_worktree_bytes(db, *mode, oid)?))
                            }
                            None => slot.worktree.clone(),
                        };
                    }
                    slot.conflict = Some(MergeConflictKind::DirRenameLocation {
                        old_path: info.old_path.clone(),
                        renamed_from: info.renamed_from.clone(),
                        added_in,
                        dir_renamed_in,
                        back_to_self,
                    });
                } else {
                    info_messages.push(MergeInfoMessage::DirRenameLocationConflict {
                        old_path: info.old_path.clone(),
                        new_path: dest.clone(),
                        renamed_from: info.renamed_from.clone(),
                        added_in,
                        dir_renamed_in,
                    });
                }
            }
        }
    }

    // Directory/file (D/F) conflict resolution (merge-ort `process_entry`): a
    // path that ends up as a *file* in the merged result while another result
    // path lives *under* it (so the path is simultaneously a directory) cannot
    // coexist. git keeps the directory at the original path and moves the file
    // aside to `path~<branch>` via `unique_path`, where `<branch>` is the side
    // that contributed the file. We resolve this on the flattened `leaves` after
    // every per-path decision is made, so renames/dir-renames have settled first.
    resolve_directory_file_conflicts(
        db,
        &mut paths,
        &mut leaves,
        &mut clean,
        &eff_ours,
        &eff_theirs,
        options,
        &mut info_messages,
    )?;

    let tree = write_merged_tree(db, &leaves)?;

    cleanup_paths.retain(|path| !leaves.contains_key(path));

    Ok(MergeTreesResult {
        tree,
        paths,
        clean,
        cleanup_paths: cleanup_paths.into_iter().collect(),
        info_messages,
    })
}

/// Flatten a branch label the way git's `add_flattened_path` does for
/// `unique_path`: any `/` in the branch name becomes `_` so the synthesized
/// `path~branch` stays a single path component family.
fn flatten_branch_label(branch: &str) -> String {
    branch.replace('/', "_")
}

/// Pick a `path~<branch>` name not already present in `leaves` (or claimed by an
/// existing `paths` entry), mirroring merge-ort's `unique_path`: start from
/// `path~branch`, then append `_0`, `_1`, … on collision.
fn unique_df_path(
    path: &[u8],
    branch: &str,
    leaves: &MergeEntryMap,
    paths: &[MergedPath],
) -> Vec<u8> {
    let mut base = path.to_vec();
    base.push(b'~');
    base.extend_from_slice(flatten_branch_label(branch).as_bytes());
    let taken = |candidate: &[u8]| {
        leaves.contains_key(candidate) || paths.iter().any(|p| p.path == candidate)
    };
    if !taken(&base) {
        return base;
    }
    let mut suffix = 0usize;
    loop {
        let mut candidate = base.clone();
        candidate.push(b'_');
        candidate.extend_from_slice(suffix.to_string().as_bytes());
        if !taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Resolve directory/file collisions in the merged leaf set. For every file leaf
/// whose path is also a directory (some other leaf lives under `path/`), move the
/// file to `path~<branch>` and record a [`MergeConflictKind::FileDirectory`].
#[allow(clippy::too_many_arguments)]
fn resolve_directory_file_conflicts(
    db: &FileObjectDatabase,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    clean: &mut bool,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
    info_messages: &mut Vec<MergeInfoMessage>,
) -> Result<()> {
    // A path is a "directory" in the result iff some leaf key has it as a strict
    // `path/` prefix. Collect every such directory prefix once.
    let mut directory_prefixes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for key in leaves.keys() {
        let mut idx = 0;
        while let Some(pos) = key[idx..].iter().position(|b| *b == b'/') {
            let end = idx + pos;
            directory_prefixes.insert(key[..end].to_vec());
            idx = end + 1;
        }
    }
    if directory_prefixes.is_empty() {
        return Ok(());
    }

    // File leaves that collide with a directory of the same name.
    let colliding: Vec<Vec<u8>> = leaves
        .keys()
        .filter(|key| directory_prefixes.contains(*key))
        .cloned()
        .collect();

    for original in colliding {
        let Some(entry) = leaves.remove(&original) else {
            continue;
        };
        // The moved-aside file must be materialized in the worktree at its new
        // path; read its blob bytes once so the porcelain has worktree content.
        let moved_bytes = merge_worktree_bytes(db, entry.0, &entry.1)?;
        // Which side contributed the file? git keys off `dirmask`: the file lives
        // on the side that is NOT the directory. We read it off the effective side
        // maps — whichever side has this path as a plain file. When only theirs has
        // it, use the theirs label; otherwise (ours has it, or both do) ours wins,
        // matching git's index-1 bias for the moved-aside name.
        let ours_has_file = eff_ours.contains_key(&original);
        let theirs_has_file = eff_theirs.contains_key(&original);
        let from_ours = ours_has_file || !theirs_has_file;
        let branch = if from_ours {
            options.ours_label
        } else {
            options.theirs_label
        };
        let new_path = unique_df_path(&original, branch, leaves, paths);
        leaves.insert(new_path.clone(), entry);
        *clean = false;

        // Relocate the path's MergedPath: update its destination and stamp the D/F
        // conflict. If the path had no MergedPath (defensive), synthesize one.
        if let Some(slot) = paths.iter_mut().find(|p| p.path == original) {
            if let Some(MergeConflictKind::RenameDelete {
                old_path,
                renamed_in,
                deleted_in,
            }) = &slot.conflict
            {
                info_messages.push(MergeInfoMessage::RenameDeleteConflict {
                    old_path: old_path.clone(),
                    new_path: original.clone(),
                    renamed_in: renamed_in.clone(),
                    deleted_in: deleted_in.clone(),
                });
            }
            slot.path = new_path.clone();
            slot.result = Some(entry);
            // Preserve any pre-existing higher-order stages; a clean file leaf has
            // none, so seed ours/theirs from the effective maps for `ls-files -u`.
            if slot.stages.base.is_none()
                && slot.stages.ours.is_none()
                && slot.stages.theirs.is_none()
            {
                slot.stages = MergeStages {
                    base: None,
                    ours: if from_ours { Some(entry) } else { None },
                    theirs: if from_ours { None } else { Some(entry) },
                };
            }
            // Keep the slot's existing `auto_merged`: git only emits
            // `Auto-merging <new_path>` for the moved file when a real content
            // merge ran (a rename or both-sides change drives filemask>=6 through
            // handle_content_merge). A plain one-sided add (filemask 2/4) is moved
            // aside silently, so we must NOT force the flag on here.
            slot.worktree = Some((entry.0, moved_bytes));
            slot.conflict = Some(MergeConflictKind::FileDirectory {
                original_path: original.clone(),
                moved_from: branch.to_string(),
            });
        } else {
            paths.push(MergedPath {
                path: new_path.clone(),
                stages: MergeStages {
                    base: None,
                    ours: if from_ours { Some(entry) } else { None },
                    theirs: if from_ours { None } else { Some(entry) },
                },
                result: Some(entry),
                worktree: Some((entry.0, moved_bytes)),
                conflict: Some(MergeConflictKind::FileDirectory {
                    original_path: original.clone(),
                    moved_from: branch.to_string(),
                }),
                auto_merged: false,
            });
        }
    }

    // Keep `paths` sorted by destination path (callers and tests assume order).
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// Construct a clean (non-conflicted) [`MergedPath`].
fn clean_path(path: Vec<u8>, result: Option<(u32, ObjectId)>) -> MergedPath {
    clean_path_auto(path, result, false)
}

/// Like [`clean_path`] but records whether the path went through a textual
/// 3-way content merge (for the "Auto-merging" message).
fn clean_path_auto(
    path: Vec<u8>,
    result: Option<(u32, ObjectId)>,
    auto_merged: bool,
) -> MergedPath {
    MergedPath {
        path,
        stages: MergeStages::default(),
        result,
        worktree: None,
        conflict: None,
        auto_merged,
    }
}

/// Snapshot the present stages for a conflicted path.
fn stages_for(
    base: &Option<(u32, ObjectId)>,
    ours: &Option<(u32, ObjectId)>,
    theirs: &Option<(u32, ObjectId)>,
) -> MergeStages {
    MergeStages {
        base: *base,
        ours: *ours,
        theirs: *theirs,
    }
}

/// Read a blob's raw bytes, requiring it to be a blob object.
fn merge_blob_bytes(reader: &impl ObjectReader, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = reader.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {}, found {}",
            oid,
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

fn merge_worktree_bytes(reader: &impl ObjectReader, mode: u32, oid: &ObjectId) -> Result<Vec<u8>> {
    if sley_index::is_gitlink(mode) {
        Ok(Vec::new())
    } else {
        merge_blob_bytes(reader, oid)
    }
}

/// 3-way merge of a file mode. Returns the resolved mode and whether the modes
/// conflict (both sides changed it to different non-base values).
fn merge_file_modes(base: Option<u32>, ours: u32, theirs: u32) -> (u32, bool) {
    if ours == theirs {
        return (ours, false);
    }
    match base {
        Some(base) if ours == base => (theirs, false),
        Some(base) if theirs == base => (ours, false),
        _ => (ours, true),
    }
}

/// Build a top-level tree object from a flat map of `path -> (mode, oid)`
/// leaves, writing every (sub)tree object to `db`.
fn write_merged_tree(db: &FileObjectDatabase, leaves: &MergeEntryMap) -> Result<ObjectId> {
    let mut root = MergeTreeNode::default();
    for (path, (mode, oid)) in leaves {
        root.insert(path, *mode, *oid);
    }
    root.write(db)
}

#[derive(Default)]
struct MergeTreeNode {
    blobs: BTreeMap<Vec<u8>, (u32, ObjectId)>,
    subtrees: BTreeMap<Vec<u8>, MergeTreeNode>,
}

impl MergeTreeNode {
    fn insert(&mut self, path: &[u8], mode: u32, oid: ObjectId) {
        match path.iter().position(|byte| *byte == b'/') {
            Some(slash) => {
                let component = path[..slash].to_vec();
                let rest = &path[slash + 1..];
                self.subtrees
                    .entry(component)
                    .or_default()
                    .insert(rest, mode, oid);
            }
            None => {
                self.blobs.insert(path.to_vec(), (mode, oid));
            }
        }
    }

    fn write(&self, db: &FileObjectDatabase) -> Result<ObjectId> {
        let mut entries: Vec<TreeEntry> = Vec::new();
        for (name, (mode, oid)) in &self.blobs {
            entries.push(TreeEntry {
                mode: *mode,
                name: BString::from(name.clone()),
                oid: *oid,
            });
        }
        for (name, subtree) in &self.subtrees {
            let oid = subtree.write(db)?;
            entries.push(TreeEntry {
                mode: 0o040000,
                name: BString::from(name.clone()),
                oid,
            });
        }
        entries.sort_by_key(merge_tree_sort_key);
        let tree = Tree { entries };
        db.write_object(EncodedObject::new(ObjectType::Tree, tree.write()))
    }
}

fn merge_tree_sort_key(entry: &TreeEntry) -> Vec<u8> {
    let mut key = entry.name.as_bytes().to_vec();
    if entry.mode == 0o040000 {
        key.push(b'/');
    }
    key
}

// --- Rename-aware non-recursive merge -------------------------------------

/// Which side of the merge performed a rename.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameSide {
    Ours,
    Theirs,
}

/// One detected one-sided rename: its source path and which side renamed it.
#[derive(Clone)]
struct MergeRename {
    source: Vec<u8>,
    side: RenameSide,
}

/// A file renamed on one side whose source was *deleted* on the other side — a
/// rename/delete conflict. git keeps the renamed content at the destination but
/// flags the merge as conflicted.
#[derive(Clone)]
struct RenameDelete {
    /// The pre-rename source path (deleted on the other side).
    source: Vec<u8>,
    /// Which side performed the rename (the other side deleted the source).
    side: RenameSide,
}

/// The rename pairings discovered for one merge: which destination paths came
/// from which source path, and which side renamed (so the other side's change
/// can follow the rename and conflict labels can be path-qualified like git).
#[derive(Default)]
struct MergeRenames {
    /// One-sided renames keyed by *destination* path. Only renames where the
    /// OTHER side kept/modified the source in place are recorded (the case
    /// where the modification must follow the rename).
    dest_to_source: BTreeMap<Vec<u8>, MergeRename>,
    /// Rename/delete conflicts: a file renamed on one side whose source the
    /// other side deleted. Keyed by destination path.
    rename_deletes: BTreeMap<Vec<u8>, RenameDelete>,
    /// Rename/rename(1to2) conflicts keyed by source path.
    rename_rename_one_to_two: BTreeMap<Vec<u8>, RenameRenameOneToTwo>,
    /// Rename/rename(2to1) conflicts keyed by the shared *destination* path:
    /// ours renamed `ours_source`->dest and theirs renamed `theirs_source`->dest.
    rename_rename_two_to_one: BTreeMap<Vec<u8>, RenameRenameTwoToOne>,
    /// Rename/add conflicts keyed by *destination*: one side renamed a file to
    /// `dest` while the other side added a different file at the same `dest`.
    rename_adds: BTreeMap<Vec<u8>, RenameAdd>,
}

#[derive(Clone)]
struct RenameRenameOneToTwo {
    ours_dest: Vec<u8>,
    theirs_dest: Vec<u8>,
}

/// A rename/rename(2to1): two distinct sources renamed onto one destination, one
/// rename per side. Each side's content at the destination is the 3-way merge of
/// its rename (the other side's change to that source follows the rename).
#[derive(Clone)]
struct RenameRenameTwoToOne {
    /// The source ours renamed onto the destination.
    ours_source: Vec<u8>,
    /// The source theirs renamed onto the destination.
    theirs_source: Vec<u8>,
}

/// A rename/add: one side renamed a file onto `dest`, the other side added an
/// unrelated file at `dest`. The renaming side's content is the 3-way merge of
/// its rename; the adding side contributes its added blob verbatim.
#[derive(Clone)]
struct RenameAdd {
    /// The pre-rename source path on the renaming side.
    source: Vec<u8>,
    /// Which side performed the rename (the other side added at `dest`).
    side: RenameSide,
}

/// Every file rename observed on one side (base->side), as `(old, new)` pairs.
/// Unlike [`MergeRenames`] this is the *complete* rename set on a side — it is
/// the input to directory-rename inference, which needs to see all the per-file
/// moves between directories, not just the ones the other side kept in place.
struct SideRenames {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Detect one-sided renames usable for a non-recursive merge: a path present in
/// `base`, deleted on one side and present (renamed) at a new path on that same
/// side, while the OTHER side still has the original path (modified or
/// unchanged). Such a rename lets the other side's change move to the
/// destination.
///
/// Also returns the complete per-side rename set so the caller can infer
/// directory renames (which need every file move, not just the merge-relevant
/// ones).
fn detect_merge_renames(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<(MergeRenames, SideRenames, SideRenames)> {
    let mut renames = MergeRenames::default();

    // Renames on ours: the other side that must carry its change is theirs.
    let ours_side = collect_side_renames(
        db,
        format,
        base_map,
        ours_map,
        theirs_map,
        RenameSide::Ours,
        options.rename_threshold,
        options.rename_limit,
        &mut renames,
    )?;
    // Renames on theirs: the other side that carries its change is ours.
    let theirs_side = collect_side_renames(
        db,
        format,
        base_map,
        theirs_map,
        ours_map,
        RenameSide::Theirs,
        options.rename_threshold,
        options.rename_limit,
        &mut renames,
    )?;

    collect_rename_rename_one_to_two(&mut renames, &ours_side, &theirs_side);
    collect_rename_rename_two_to_one_and_adds(
        &mut renames,
        &ours_side,
        &theirs_side,
        base_map,
        ours_map,
        theirs_map,
    );

    Ok((renames, ours_side, theirs_side))
}

/// Detect rename/rename(2to1) and rename/add conflicts from the complete per-side
/// rename sets. Both arise when a one-sided rename's destination is *occupied* on
/// the other side (so [`collect_side_renames`] left it out of `dest_to_source`):
///
/// * 2to1 — both sides renamed (distinct sources) onto the same destination.
/// * rename/add — one side renamed onto a path the other side *added* fresh
///   (the destination is new to the other side, not a base path it kept and not
///   itself a rename destination on that side).
fn collect_rename_rename_two_to_one_and_adds(
    renames: &mut MergeRenames,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
) {
    let ours_by_dest: BTreeMap<&[u8], &[u8]> = ours_side
        .pairs
        .iter()
        .map(|(old, new)| (new.as_slice(), old.as_slice()))
        .collect();
    let theirs_by_dest: BTreeMap<&[u8], &[u8]> = theirs_side
        .pairs
        .iter()
        .map(|(old, new)| (new.as_slice(), old.as_slice()))
        .collect();

    // 2to1: a destination that is a rename target on BOTH sides from different
    // sources. (Same source on both sides is a rename/rename(1to1), handled by
    // the path-keyed core; same source to two dests is the 1to2 case above.)
    for (dest, ours_src) in &ours_by_dest {
        let Some(theirs_src) = theirs_by_dest.get(dest) else {
            continue;
        };
        if ours_src == theirs_src {
            continue;
        }
        // Don't disturb a destination the 1to2 pass already claimed.
        if renames.rename_rename_one_to_two.contains_key(*dest) {
            continue;
        }
        renames.rename_rename_two_to_one.insert(
            dest.to_vec(),
            RenameRenameTwoToOne {
                ours_source: ours_src.to_vec(),
                theirs_source: theirs_src.to_vec(),
            },
        );
    }

    // rename/add on ours: ours renamed onto `dest`, which theirs added (present
    // on theirs, absent from base, and not a theirs rename target).
    for (dest, ours_src) in &ours_by_dest {
        if renames.rename_rename_two_to_one.contains_key(*dest)
            || renames.rename_rename_one_to_two.contains_key(*dest)
        {
            continue;
        }
        if theirs_map.contains_key(*dest)
            && !base_map.contains_key(*dest)
            && !theirs_by_dest.contains_key(dest)
        {
            renames.rename_adds.insert(
                dest.to_vec(),
                RenameAdd {
                    source: ours_src.to_vec(),
                    side: RenameSide::Ours,
                },
            );
        }
    }
    // rename/add on theirs: symmetric.
    for (dest, theirs_src) in &theirs_by_dest {
        if renames.rename_rename_two_to_one.contains_key(*dest)
            || renames.rename_rename_one_to_two.contains_key(*dest)
            || renames.rename_adds.contains_key(*dest)
        {
            continue;
        }
        if ours_map.contains_key(*dest)
            && !base_map.contains_key(*dest)
            && !ours_by_dest.contains_key(dest)
        {
            renames.rename_adds.insert(
                dest.to_vec(),
                RenameAdd {
                    source: theirs_src.to_vec(),
                    side: RenameSide::Theirs,
                },
            );
        }
    }
}

fn collect_rename_rename_one_to_two(
    renames: &mut MergeRenames,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
) {
    let ours_by_source: BTreeMap<&[u8], &[u8]> = ours_side
        .pairs
        .iter()
        .map(|(old, new)| (old.as_slice(), new.as_slice()))
        .collect();
    for (old, theirs_new) in &theirs_side.pairs {
        let Some(ours_new) = ours_by_source.get(old.as_slice()) else {
            continue;
        };
        if *ours_new == theirs_new.as_slice() {
            continue;
        }
        renames.rename_deletes.remove(*ours_new);
        renames.rename_deletes.remove(theirs_new);
        renames.dest_to_source.remove(*ours_new);
        renames.dest_to_source.remove(theirs_new);
        renames.rename_rename_one_to_two.insert(
            old.clone(),
            RenameRenameOneToTwo {
                ours_dest: (*ours_new).to_vec(),
                theirs_dest: theirs_new.clone(),
            },
        );
    }
}

/// Collect renames that occurred on `side` (relative to `base`). Records the
/// merge-relevant subset (renames the `other` side still references) into
/// `renames`, and returns the *complete* per-side rename set for directory-rename
/// inference. `db`/`format` resolve blob bytes for similarity scoring.
#[allow(clippy::too_many_arguments)]
fn collect_side_renames(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base_map: &MergeEntryMap,
    side_map: &MergeEntryMap,
    other_map: &MergeEntryMap,
    side: RenameSide,
    threshold: u8,
    rename_limit: usize,
    renames: &mut MergeRenames,
) -> Result<SideRenames> {
    // Diff base->side with inexact rename detection; the resulting `Renamed`
    // entries name (old_path -> new_path) pairs on this side.
    let base_tree = entry_map_as_tracked(base_map);
    let side_tree = entry_map_as_tracked(side_map);
    let options = DiffNameStatusOptions {
        detect_renames: true,
        detect_copies: false,
        find_copies_harder: false,
        rename_empty: false,
        detect_inexact: true,
        rename_threshold: threshold,
        copy_threshold: threshold,
        rename_limit,
        ..Default::default()
    };
    let changes = diff_name_status_maps_with_renames(
        &base_tree,
        &side_tree,
        base_tree.keys().chain(side_tree.keys()),
        options,
        |oid| merge_blob_bytes(db, oid).ok().map(|b| Arc::from(b.into_boxed_slice())),
    )?;

    let mut pairs = Vec::new();
    for change in changes {
        let NameStatus::Renamed(_) = change.status else {
            continue;
        };
        let Some(old_path) = change.old_path.as_ref() else {
            continue;
        };
        let old = old_path.as_bytes().to_vec();
        let new = change.path.as_bytes().to_vec();
        // Complete rename set, fed to directory-rename inference.
        pairs.push((old.clone(), new.clone()));

        // Only act when the destination is genuinely new (not already present
        // in either side from a different origin) and the OTHER side still
        // references the source path — i.e. the other side modified/kept `old`,
        // and its change should follow the rename to `new`.
        if !other_map.contains_key(&old) {
            // The source path is gone on the other side. If it existed in base
            // (so the other side *deleted* it) and the other side did not also
            // produce `new`, this is a rename/delete conflict: this side renamed
            // the file, the other side deleted its source.
            if base_map.contains_key(&old) && !other_map.contains_key(&new) {
                renames
                    .rename_deletes
                    .entry(new.clone())
                    .or_insert(RenameDelete {
                        source: old.clone(),
                        side,
                    });
            }
            continue;
        }
        // If the other side ALSO renamed/created `new`, that is a rename/rename
        // or rename/add corner case we leave to the path-keyed core (stage-b).
        if other_map.contains_key(&new) {
            continue;
        }
        // Skip if both sides renamed the same source to the same dest (already
        // recorded) or to anything (first writer wins; the path-keyed core then
        // sees identical dest entries and resolves trivially).
        renames
            .dest_to_source
            .entry(new)
            .or_insert(MergeRename { source: old, side });
    }

    let _ = format;
    Ok(SideRenames { pairs })
}

/// Rewrite the three side maps so that each detected one-sided rename old->new
/// presents the OTHER side's `old` entry at `new`, and removes `old` from
/// every side. The path-keyed merge core then performs the 3-way content merge
/// at `new` with base=base[old], one side = the renaming side's new content,
/// the other side = the modifying side's old content.
fn apply_merge_renames(
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    renames: &MergeRenames,
) -> (MergeEntryMap, MergeEntryMap, MergeEntryMap) {
    if renames.dest_to_source.is_empty() {
        return (base_map.clone(), ours_map.clone(), theirs_map.clone());
    }
    let mut base = base_map.clone();
    let mut ours = ours_map.clone();
    let mut theirs = theirs_map.clone();

    for (new, rename) in &renames.dest_to_source {
        let old = &rename.source;
        // Move base[old] to base[new] so the destination has a proper ancestor.
        if let Some(entry) = base.remove(old) {
            base.entry(new.clone()).or_insert(entry);
        }
        // For each side, if it still has `old`, move that entry to `new`.
        for side in [&mut ours, &mut theirs] {
            if let Some(entry) = side.remove(old) {
                side.entry(new.clone()).or_insert(entry);
            }
        }
    }
    (base, ours, theirs)
}

// --- Directory-rename detection -------------------------------------------

/// The parent directory of `path`, or `None` for a top-level path.
fn parent_dir(path: &[u8]) -> Option<&[u8]> {
    path.iter().rposition(|b| *b == b'/').map(|i| &path[..i])
}

/// Apply a directory rename `old_dir -> new_dir` to `path` (which must live
/// under `old_dir`). E.g. `old_dir=z`, `new_dir=y`, `path=z/d` -> `y/d`; an
/// empty `new_dir` (rename into the repo root) drops the directory prefix.
fn apply_dir_rename(old_dir: &[u8], new_dir: &[u8], path: &[u8]) -> Vec<u8> {
    // The portion of `path` after `old_dir/` (handle root-target by stepping
    // past the separator, exactly as git's apply_dir_rename does).
    let rest_start = if new_dir.is_empty() {
        old_dir.len() + 1
    } else {
        old_dir.len()
    };
    let mut out = new_dir.to_vec();
    out.extend_from_slice(&path[rest_start..]);
    out
}

/// Find the longest renamed ancestor directory of `path`: walk parent dirs from
/// the deepest up and return the first one present in `dir_renames`. Mirrors
/// merge-ort's `check_dir_renamed`.
fn check_dir_renamed<'a>(
    path: &[u8],
    dir_renames: &'a BTreeMap<Vec<u8>, Vec<u8>>,
) -> Option<(&'a [u8], &'a [u8])> {
    let mut cur = parent_dir(path);
    while let Some(dir) = cur {
        if let Some((old_dir, new_dir)) = dir_renames.get_key_value(dir) {
            return Some((old_dir.as_slice(), new_dir.as_slice()));
        }
        cur = parent_dir(dir);
    }
    None
}

/// The provisional directory renames computed for both sides, plus the source
/// directories whose rename was ambiguous (a "split").
struct DirectoryRenameMaps {
    /// `old_dir -> new_dir` directory renames detected on ours' side. A path
    /// added/renamed by theirs under `old_dir` re-homes into `new_dir`.
    ours: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Directory renames detected on theirs' side.
    theirs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Source directories whose split was unclear on ours' side (no unique
    /// majority target); paths on theirs that would need to follow such a rename
    /// are a conflict, not silent.
    ours_split: BTreeSet<Vec<u8>>,
    /// Source directories whose split was unclear on theirs' side.
    theirs_split: BTreeSet<Vec<u8>>,
}

/// Infer directory renames from the complete per-side file-rename sets, mirroring
/// merge-ort's `get_provisional_directory_renames` + `handle_directory_level_conflicts`.
/// For every file moved `.../old_dir/x -> .../new_dir/x`, the ancestor pairs are
/// tallied (`dir_rename_count`) and collapsed to `old_dir -> best_new_dir` where
/// `best` is the unique highest count. A tie marks the source directory as a
/// "split". A rename is only kept if the source directory was *entirely removed*
/// on that side (the `dirs_removed` gate). A directory renamed on BOTH sides is
/// dropped from both maps (ambiguous).
fn compute_directory_renames(
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
) -> DirectoryRenameMaps {
    let ours = compute_side_dir_renames(&ours_side.pairs, ours_map);
    let theirs = compute_side_dir_renames(&theirs_side.pairs, theirs_map);

    // A directory renamed on BOTH sides (to whatever target) is ambiguous;
    // git's handle_directory_level_conflicts drops it from both maps so neither
    // side's directory rename is applied.
    let mut ours_map_out = ours.renames;
    let mut theirs_map_out = theirs.renames;
    let dup: Vec<Vec<u8>> = ours_map_out
        .keys()
        .filter(|k| theirs_map_out.contains_key(*k))
        .cloned()
        .collect();
    for k in dup {
        ours_map_out.remove(&k);
        theirs_map_out.remove(&k);
    }

    DirectoryRenameMaps {
        ours: ours_map_out,
        theirs: theirs_map_out,
        ours_split: ours.split,
        theirs_split: theirs.split,
    }
}

/// Per-side directory-rename computation result.
struct SideDirRenames {
    renames: BTreeMap<Vec<u8>, Vec<u8>>,
    split: BTreeSet<Vec<u8>>,
}

/// Compute one side's `old_dir -> new_dir` map from its file renames, gated on
/// the source directory being fully removed on that side.
fn compute_side_dir_renames(
    pairs: &[(Vec<u8>, Vec<u8>)],
    side_map: &MergeEntryMap,
) -> SideDirRenames {
    // dir_rename_count: count[old_dir][new_dir]. Built by walking every rename's
    // ancestor directories while the *trailing* path components match, exactly
    // as merge-ort's update_dir_rename_counts does. For
    //   a/b/c/d/e/foo.c -> a/b/some/thing/else/e/foo.c
    // this records both
    //   a/b/c/d/e => a/b/some/thing/else/e   AND   a/b/c/d => a/b/some/thing/else
    // but stops once the trailing components diverge.
    let mut counts: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>> = BTreeMap::new();
    for (old, new) in pairs {
        update_dir_rename_counts(&mut counts, old, new);
    }

    let mut renames = BTreeMap::new();
    let mut split = BTreeSet::new();
    for (old_dir, targets) in counts {
        let mut max = 0usize;
        let mut bad_max = 0usize;
        let mut best: Option<Vec<u8>> = None;
        for (target, count) in &targets {
            if *count == max {
                bad_max = max;
            } else if *count > max {
                max = *count;
                best = Some(target.clone());
            }
        }
        if max == 0 {
            continue;
        }
        if bad_max == max {
            split.insert(old_dir);
            continue;
        }
        // dirs_removed gate: the source directory must be entirely gone on this
        // side. New files that recreate the old directory count too; otherwise
        // cases like "both sides renamed z/ -> y/, but one side added z/d"
        // incorrectly look like both sides performed a whole-directory rename.
        if let Some(best) = best
            && directory_fully_removed(&old_dir, side_map)
        {
            renames.insert(old_dir, best);
        }
    }

    SideDirRenames { renames, split }
}

/// Tally the ancestor directory-rename pairs implied by a single file rename
/// `old -> new`, mirroring merge-ort's `update_dir_rename_counts`. Starting from
/// the immediate parent dirs, we strip one trailing component at a time and
/// record `old_ancestor -> new_ancestor` as long as the *remaining* trailing
/// suffix still matches between the two paths.
fn update_dir_rename_counts(
    counts: &mut BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>>,
    old: &[u8],
    new: &[u8],
) {
    // Work on owned copies we progressively truncate at each '/'.
    let mut old_dir = old.to_vec();
    let mut new_dir = new.to_vec();
    let mut first = true;
    loop {
        // Strip the trailing component (basename on the first pass, then a dir
        // each pass) to ascend one level.
        let old_has = dir_munge(&mut old_dir);
        let new_has = dir_munge(&mut new_dir);

        // On the first pass we only stripped the basename; the dirs need not
        // match. On later passes the *trailing* components must agree, otherwise
        // the rename no longer implies this ancestor pairing.
        if !first {
            let old_sub = trailing_component(old, &old_dir);
            let new_sub = trailing_component(new, &new_dir);
            if old_sub != new_sub {
                break;
            }
        }

        if old_dir == new_dir {
            // Same directory at this level — no rename implied, and no deeper
            // ancestor can differ usefully either.
            break;
        }
        *counts
            .entry(old_dir.clone())
            .or_default()
            .entry(new_dir.clone())
            .or_default() += 1;

        first = false;
        // Hitting the toplevel ("") on either side ends the ascent.
        if old_dir.is_empty() || new_dir.is_empty() {
            break;
        }
        // If the two ancestors are identical from here up, stop (git stops once
        // the suffix-equal walk reaches a common prefix).
        if !old_has || !new_has {
            break;
        }
    }
}

/// Truncate `buf` at its last '/', leaving the parent directory (or empty for a
/// toplevel name). Returns whether a '/' was present (i.e. there is a deeper
/// ancestor to ascend into).
fn dir_munge(buf: &mut Vec<u8>) -> bool {
    match buf.iter().rposition(|b| *b == b'/') {
        Some(i) => {
            buf.truncate(i);
            true
        }
        None => {
            buf.clear();
            false
        }
    }
}

/// The trailing path component that was stripped from `full` to reach `dir`
/// (i.e. the suffix of `full` after `dir/`). Used to compare whether the two
/// sides of a rename share the same trailing directory chain.
fn trailing_component<'a>(full: &'a [u8], dir: &[u8]) -> &'a [u8] {
    if dir.is_empty() {
        full
    } else {
        // full = dir + "/" + suffix
        &full[dir.len() + 1..]
    }
}

/// True when no path under `dir/` exists on `side` (the directory was entirely
/// removed there). Mirrors merge-ort's `dirs_removed` precondition.
fn directory_fully_removed(dir: &[u8], side_map: &MergeEntryMap) -> bool {
    let mut prefix = dir.to_vec();
    prefix.push(b'/');
    for path in side_map.keys() {
        if path.starts_with(&prefix) {
            return false;
        }
    }
    true
}

/// A path on one side whose location is rewritten by a directory rename the
/// *other* side performed. The rewrite applies equally to a freshly added file
/// and to a file the side itself renamed (a transitive rename).
struct DirRenameMove {
    /// The path as it currently sits in the side's effective map (the side's own
    /// rename, if any, already applied).
    from: Vec<u8>,
    /// The re-homed destination, after applying the other side's directory rename.
    to: Vec<u8>,
    /// `Some(source)` when `from` is a rename destination produced by this side
    /// (transitive rename); `None` for a fresh add. Drives git's
    /// "renamed to"/"added in" message wording.
    renamed_from: Option<Vec<u8>>,
}

struct DirRenameTwoToOne {
    dest: Vec<u8>,
    ours_source: Vec<u8>,
    theirs_source: Vec<u8>,
    ours_label_path: Vec<u8>,
    theirs_label_path: Vec<u8>,
}

/// Provenance of a re-homed path, for `=conflict`-mode `CONFLICT (file location)`
/// reporting.
#[derive(Clone)]
struct RehomeInfo {
    /// The pre-re-home path on the adding/renaming side.
    old_path: Vec<u8>,
    /// `Some(source)` for a transitive rename, `None` for a fresh add.
    renamed_from: Option<Vec<u8>>,
    /// Whether the *adding/renaming* side was ours (true) or theirs (false). The
    /// caller resolves this to a branch label.
    added_on_ours: bool,
}

/// Per-side provenance for a destination created by directory-rename rehoming.
#[derive(Clone, Default)]
struct RehomeSides {
    ours: Option<RehomeInfo>,
    theirs: Option<RehomeInfo>,
}

/// An implicit-directory-rename collision: one or more paths a directory rename
/// would re-home onto `dest`, which is blocked because `dest` is already
/// occupied (a file in the way) or because multiple sources map to it. git emits
/// `CONFLICT (implicit dir rename): Existing file/dir at <dest> in the way ...`.
struct DirRenameCollision {
    /// The blocked destination path (the file/dir already there).
    dest: Vec<u8>,
    /// The source path(s) the directory rename tried to move onto `dest`.
    sources: Vec<Vec<u8>>,
}

/// Outcome of applying directory renames to all three effective maps.
struct DirRenameOutcome {
    /// Rewritten base/ours/theirs maps with re-homed paths moved to their
    /// destinations. `base` moves too so a re-homed content-merge keeps its
    /// ancestor at the new location.
    base: MergeEntryMap,
    ours: MergeEntryMap,
    theirs: MergeEntryMap,
    /// Re-homed destination path -> provenance (for `=conflict`-mode reporting).
    rehomed: BTreeMap<Vec<u8>, RehomeSides>,
    /// Implicit-dir-rename collisions (file in the way / N-to-1), for the
    /// `CONFLICT (implicit dir rename)` message; always conflicts regardless of
    /// mode.
    collisions: Vec<DirRenameCollision>,
    /// Split source dirs that were relevant to a path on the other side.
    splits: BTreeSet<Vec<u8>>,
    /// Destinations where a directory rename moved a file back onto its own base
    /// source path (rename-to-self) and the other side modified that path. git
    /// records these as an unmerged file-location conflict (`UU`) rather than a
    /// clean auto-resolution; the trivial 3-way at the destination would
    /// otherwise resolve cleanly because the renamed side's content equals base.
    back_to_self: BTreeSet<Vec<u8>>,
    /// True if a directory-level collision or split made the merge dirty even in
    /// `=true` mode (e.g. two paths re-homed onto one destination).
    dirty: bool,
    info_messages: Vec<MergeInfoMessage>,
}

/// Apply directory renames to both sides' effective maps.
///
/// This mirrors merge-ort's `collect_renames` + `check_for_directory_rename` +
/// `apply_directory_rename_modifications`: every path a side *added* or *renamed*
/// that lives under a directory the OTHER side renamed has its destination
/// rewritten to follow that rename — making the directory rename a property of
/// the rename-detection pass that every path consults, not a per-file special
/// case. Handles:
///   - transitive renames (a file the side renamed into a dir the other side
///     renamed follows on into the final directory),
///   - `dir_rename_exclusions` (never re-home into a directory THIS side itself
///     renamed — that would create a spurious rename/rename(1to2)),
///   - collisions (N paths mapping to one destination -> conflict),
///   - splits (a source dir with no majority target -> conflict, leave in place).
#[allow(clippy::too_many_arguments)]
fn apply_directory_renames(
    base_map: &MergeEntryMap,
    eff_base: &MergeEntryMap,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    ours_side: &SideRenames,
    theirs_side: &SideRenames,
    dir_renames: &DirectoryRenameMaps,
    file_rename_dests: &BTreeMap<Vec<u8>, MergeRename>,
) -> DirRenameOutcome {
    let mut base = eff_base.clone();
    let mut ours = eff_ours.clone();
    let mut theirs = eff_theirs.clone();
    let mut rehomed = BTreeMap::new();
    let mut collisions = Vec::new();
    let mut splits = BTreeSet::new();
    let mut back_to_self = BTreeSet::new();
    let mut info_messages = Vec::new();
    let mut dirty = false;

    // Ours' paths follow THEIRS' directory renames; the exclusions are OURS' own
    // renamed-into dirs (never re-home a path into a directory this same side
    // renamed). Symmetrically for theirs.
    let ours_excl = exclusion_dirs(&dir_renames.ours);
    let theirs_excl = exclusion_dirs(&dir_renames.theirs);

    // Plan ours' moves (following theirs' dir-renames) and theirs' moves
    // (following ours' dir-renames). Planning before applying lets us detect
    // collisions (N paths onto one destination) across the whole side.
    let ours_moves = plan_rehome(
        base_map,
        &ours,
        ours_side,
        &dir_renames.theirs,
        &ours_excl,
        &dir_renames.theirs_split,
        &mut collisions,
        &mut splits,
        &mut info_messages,
        &mut dirty,
    );
    let theirs_moves = plan_rehome(
        base_map,
        &theirs,
        theirs_side,
        &dir_renames.ours,
        &theirs_excl,
        &dir_renames.ours_split,
        &mut collisions,
        &mut splits,
        &mut info_messages,
        &mut dirty,
    );

    apply_rehome_moves(
        base_map,
        file_rename_dests,
        &mut base,
        &mut ours,
        &mut theirs,
        ours_moves,
        true,
        &mut rehomed,
        &mut collisions,
        &mut back_to_self,
        &mut dirty,
    );
    apply_rehome_moves(
        base_map,
        file_rename_dests,
        &mut base,
        &mut ours,
        &mut theirs,
        theirs_moves,
        false,
        &mut rehomed,
        &mut collisions,
        &mut back_to_self,
        &mut dirty,
    );

    DirRenameOutcome {
        base,
        ours,
        theirs,
        rehomed,
        collisions,
        splits,
        back_to_self,
        dirty,
        info_messages,
    }
}

/// The set of *source* directories a side renamed away from. A directory rename
/// the other side wants to apply into one of these dirs is skipped (it would
/// produce a spurious rename/rename(1to2)); git's `dir_rename_exclusions`.
fn exclusion_dirs(side_dir_renames: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeSet<Vec<u8>> {
    side_dir_renames.keys().cloned().collect()
}

/// Re-home `target`'s added/renamed paths that fall under a directory the other
/// side renamed (`renamer_dirs`: `old_dir -> new_dir`).
///
/// Candidates are paths present on this side and absent in base — i.e. both
/// Plan the directory-rename moves for one side: which of its added/renamed
/// paths re-home where, following `renamer_dirs` (the OTHER side's dir-renames).
///
/// Candidates are paths present on this side and absent in base — both freshly
/// added files AND this side's own rename destinations (the latter give the
/// transitive-rename behaviour). A candidate whose target directory is in
/// `exclusions` (a dir this side itself renamed) is skipped. Splits mark the
/// merge dirty; N-to-1 collisions (multiple sources onto one destination) record
/// a `DirRenameCollision` and yield no move. Returns the surviving single moves
/// (one per destination).
#[allow(clippy::too_many_arguments)]
fn plan_rehome(
    base_map: &MergeEntryMap,
    side: &MergeEntryMap,
    side_renames: &SideRenames,
    renamer_dirs: &BTreeMap<Vec<u8>, Vec<u8>>,
    exclusions: &BTreeSet<Vec<u8>>,
    split_dirs: &BTreeSet<Vec<u8>>,
    collisions: &mut Vec<DirRenameCollision>,
    splits: &mut BTreeSet<Vec<u8>>,
    info_messages: &mut Vec<MergeInfoMessage>,
    dirty: &mut bool,
) -> Vec<DirRenameMove> {
    if renamer_dirs.is_empty() && split_dirs.is_empty() {
        return Vec::new();
    }

    // This side's rename destinations -> sources; eligible for a transitive
    // rewrite and carry the original source for message wording.
    let side_rename_src: BTreeMap<&[u8], &[u8]> = side_renames
        .pairs
        .iter()
        .map(|(o, n)| (n.as_slice(), o.as_slice()))
        .collect();

    let candidates: Vec<Vec<u8>> = side
        .keys()
        .filter(|p| !base_map.contains_key(*p) || side_rename_src.contains_key(p.as_slice()))
        .cloned()
        .collect();

    // dest -> the moves wanting to land there (collision detection).
    let mut planned: BTreeMap<Vec<u8>, Vec<DirRenameMove>> = BTreeMap::new();
    for path in candidates {
        if let Some(split_dir) = check_dir_split(&path, split_dirs) {
            splits.insert(split_dir.to_vec());
            *dirty = true;
            continue;
        }
        let Some((old_dir, new_dir)) = check_dir_renamed(&path, renamer_dirs) else {
            continue;
        };
        // dir_rename_exclusions: don't apply a rename INTO a directory this side
        // itself renamed; that would cause a spurious rename/rename(1to2). The
        // file instead follows this side's own rename, so leave it.
        let new_dir_is_exclusion = exclusions.contains(new_dir);
        let new_dir_inside_exclusion = exclusions
            .iter()
            .any(|dir| directory_contains_proper(dir, new_dir));
        if new_dir_is_exclusion
            || (new_dir_inside_exclusion
                && !side_has_pure_add_under_dir(side, base_map, &side_rename_src, old_dir))
        {
            info_messages.push(MergeInfoMessage::DirRenameSkippedDueToRerename {
                old_dir: old_dir.to_vec(),
                path: path.clone(),
                new_dir: new_dir.to_vec(),
            });
            continue;
        }
        let dest = apply_dir_rename(old_dir, new_dir, &path);
        if dest == path {
            // Directory rename causes a rename-to-self: already in place.
            continue;
        }
        let renamed_from = side_rename_src.get(path.as_slice()).map(|s| s.to_vec());
        planned
            .entry(dest.clone())
            .or_default()
            .push(DirRenameMove {
                from: path,
                to: dest,
                renamed_from,
            });
    }

    let mut moves = Vec::new();
    for (dest, group) in planned {
        if group.len() > 1 {
            // Multiple paths map to one destination: an implicit-dir-rename
            // collision. git leaves all of them in place and conflicts.
            *dirty = true;
            collisions.push(DirRenameCollision {
                dest,
                sources: group.into_iter().map(|m| m.from).collect(),
            });
            continue;
        }
        moves.push(group.into_iter().next().expect("non-empty"));
    }
    moves
}

fn check_dir_split<'a>(path: &[u8], split_dirs: &'a BTreeSet<Vec<u8>>) -> Option<&'a [u8]> {
    let mut dir = parent_dir(path)?;
    loop {
        if let Some(split_dir) = split_dirs.get(dir) {
            return Some(split_dir);
        }
        dir = parent_dir(dir)?;
    }
}

fn directory_contains_proper(parent: &[u8], child: &[u8]) -> bool {
    !parent.is_empty()
        && child.len() > parent.len()
        && child.starts_with(parent)
        && child[parent.len()] == b'/'
}

fn side_has_pure_add_under_dir(
    side: &MergeEntryMap,
    base_map: &MergeEntryMap,
    side_rename_src: &BTreeMap<&[u8], &[u8]>,
    dir: &[u8],
) -> bool {
    side.keys().any(|path| {
        path_is_under_dir(path, dir)
            && !base_map.contains_key(path)
            && !side_rename_src.contains_key(path.as_slice())
    })
}

fn path_is_under_dir(path: &[u8], dir: &[u8]) -> bool {
    !dir.is_empty() && path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == b'/'
}

/// Apply a side's planned re-home moves to all three effective maps.
///
/// `side_is_ours` says whether the moves originate from ours' (true) or theirs'
/// (false) paths — used both for `=conflict`-mode provenance and to decide which
/// side's entry the move primarily belongs to. A move whose source is a
/// content-merge path (present on the other side and in base too) re-homes
/// across `base`/`ours`/`theirs` together, so the 3-way merge follows it to the
/// new location; a pure add re-homes only its own side.
#[allow(clippy::too_many_arguments)]
fn apply_rehome_moves(
    original_base: &MergeEntryMap,
    file_rename_dests: &BTreeMap<Vec<u8>, MergeRename>,
    base: &mut MergeEntryMap,
    ours: &mut MergeEntryMap,
    theirs: &mut MergeEntryMap,
    moves: Vec<DirRenameMove>,
    side_is_ours: bool,
    rehomed: &mut BTreeMap<Vec<u8>, RehomeSides>,
    collisions: &mut Vec<DirRenameCollision>,
    back_to_self: &mut BTreeSet<Vec<u8>>,
    dirty: &mut bool,
) {
    for mv in moves {
        // A file in the way at the destination is only a blocker when it is
        // present on this same side (or in base). If the other side already
        // occupies the destination, applying this move produces the normal
        // two-sided conflict at that path (e.g. t6423 1d's rename/rename(2to1)).
        let occupied_on_this_side = if side_is_ours {
            ours.contains_key(&mv.to) || map_has_directory_at(ours, &mv.to)
        } else {
            theirs.contains_key(&mv.to) || map_has_directory_at(theirs, &mv.to)
        };
        let occupied_by_cross_rename =
            file_rename_dests
                .get(&mv.to)
                .is_some_and(|rename| match (side_is_ours, rename.side) {
                    (true, RenameSide::Theirs) | (false, RenameSide::Ours) => true,
                    (true, RenameSide::Ours) | (false, RenameSide::Theirs) => false,
                });
        let base_entry_at_dest = original_base.get(&mv.to).copied();
        let base_entry_at_source = original_base.get(&mv.from).copied();
        let other_side_entry_at_dest = if side_is_ours {
            theirs.get(&mv.to).copied()
        } else {
            ours.get(&mv.to).copied()
        };
        let other_side_entry_at_source = if side_is_ours {
            theirs.get(&mv.from).copied()
        } else {
            ours.get(&mv.from).copied()
        };
        let base_entry_for_shifted_source = base_entry_at_source.or(base_entry_at_dest);
        let rename_back_to_modified_source = mv
            .renamed_from
            .as_ref()
            .is_some_and(|source| source == &mv.to)
            && base_entry_at_dest.is_some()
            && (other_side_entry_at_dest.is_some_and(|entry| Some(entry) != base_entry_at_dest)
                || other_side_entry_at_source
                    .is_some_and(|entry| Some(entry) != base_entry_for_shifted_source));
        if ((base_entry_at_dest.is_some() && !rename_back_to_modified_source)
            || (occupied_on_this_side && !occupied_by_cross_rename))
            && mv.to != mv.from
        {
            *dirty = true;
            collisions.push(DirRenameCollision {
                dest: mv.to.clone(),
                sources: vec![mv.from.clone()],
            });
            continue;
        }
        let mut moved = false;
        if occupied_by_cross_rename {
            base.remove(&mv.from);
            if side_is_ours {
                if let Some(entry) = ours.remove(&mv.from) {
                    ours.insert(mv.to.clone(), entry);
                    moved = true;
                }
                theirs.remove(&mv.from);
            } else {
                ours.remove(&mv.from);
                if let Some(entry) = theirs.remove(&mv.from) {
                    theirs.insert(mv.to.clone(), entry);
                    moved = true;
                }
            }
        } else {
            // Move the path on every map that holds it (base for the ancestor,
            // and whichever sides carry content at the path). This keeps a
            // content-merge keyed consistently at the re-homed destination.
            for m in [&mut *base, &mut *ours, &mut *theirs] {
                if let Some(entry) = m.remove(&mv.from) {
                    m.insert(mv.to.clone(), entry);
                    moved = true;
                }
            }
        }
        if moved {
            if rename_back_to_modified_source {
                back_to_self.insert(mv.to.clone());
            }
            let info = RehomeInfo {
                old_path: mv.from.clone(),
                renamed_from: mv.renamed_from.clone(),
                added_on_ours: side_is_ours,
            };
            let entry = rehomed.entry(mv.to.clone()).or_default();
            if side_is_ours {
                entry.ours = Some(info);
            } else {
                entry.theirs = Some(info);
            }
        }
    }
}

fn collect_dir_rename_two_to_one(
    renames: &MergeRenames,
    rehomed: &BTreeMap<Vec<u8>, RehomeSides>,
) -> Vec<DirRenameTwoToOne> {
    let mut conflicts = Vec::new();
    for (dest, sides) in rehomed {
        let Some(file_rename) = renames.dest_to_source.get(dest) else {
            continue;
        };
        match file_rename.side {
            RenameSide::Ours => {
                let Some(info) = sides.theirs.as_ref() else {
                    continue;
                };
                let Some(theirs_source) = info.renamed_from.as_ref() else {
                    continue;
                };
                conflicts.push(DirRenameTwoToOne {
                    dest: dest.clone(),
                    ours_source: file_rename.source.clone(),
                    theirs_source: theirs_source.clone(),
                    ours_label_path: dest.clone(),
                    theirs_label_path: info.old_path.clone(),
                });
            }
            RenameSide::Theirs => {
                let Some(info) = sides.ours.as_ref() else {
                    continue;
                };
                let Some(ours_source) = info.renamed_from.as_ref() else {
                    continue;
                };
                conflicts.push(DirRenameTwoToOne {
                    dest: dest.clone(),
                    ours_source: ours_source.clone(),
                    theirs_source: file_rename.source.clone(),
                    ours_label_path: info.old_path.clone(),
                    theirs_label_path: dest.clone(),
                });
            }
        }
    }
    conflicts
}

fn map_has_directory_at(map: &MergeEntryMap, path: &[u8]) -> bool {
    let mut prefix = path.to_vec();
    prefix.push(b'/');
    map.keys().any(|candidate| candidate.starts_with(&prefix))
}

fn remap_rename_destinations(renames: &mut MergeRenames, rehomed: &BTreeMap<Vec<u8>, RehomeSides>) {
    if rehomed.is_empty() {
        return;
    }
    let mut remapped_deletes = BTreeMap::new();
    for (dest, rd) in std::mem::take(&mut renames.rename_deletes) {
        let new_dest = rehomed
            .iter()
            .find_map(|(new_dest, sides)| {
                let moved = sides
                    .ours
                    .as_ref()
                    .is_some_and(|info| info.old_path == dest)
                    || sides
                        .theirs
                        .as_ref()
                        .is_some_and(|info| info.old_path == dest);
                moved.then(|| new_dest.clone())
            })
            .unwrap_or(dest);
        remapped_deletes.insert(new_dest, rd);
    }
    renames.rename_deletes = remapped_deletes;

    for rename in renames.rename_rename_one_to_two.values_mut() {
        for (dest, sides) in rehomed {
            if sides
                .ours
                .as_ref()
                .is_some_and(|info| info.old_path == rename.ours_dest)
            {
                rename.ours_dest = dest.clone();
            }
            if sides
                .theirs
                .as_ref()
                .is_some_and(|info| info.old_path == rename.theirs_dest)
            {
                rename.theirs_dest = dest.clone();
            }
        }
    }
}

fn drop_collapsed_rename_rename_conflicts(renames: &mut MergeRenames) {
    renames
        .rename_rename_one_to_two
        .retain(|_, rename| rename.ours_dest != rename.theirs_dest);
}

fn apply_dir_rename_two_to_one_conflicts(
    db: &FileObjectDatabase,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    conflicts: &[DirRenameTwoToOne],
    paths: &mut [MergedPath],
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    for conflict in conflicts {
        let Some(slot) = paths.iter_mut().find(|path| path.path == conflict.dest) else {
            continue;
        };
        let ours_entry = eff_ours.get(&conflict.dest).copied();
        let theirs_entry = eff_theirs.get(&conflict.dest).copied();
        let (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) =
            (ours_entry, theirs_entry)
        else {
            continue;
        };
        let ours_bytes = merge_blob_bytes(db, &ours_oid)?;
        let theirs_bytes = merge_blob_bytes(db, &theirs_oid)?;
        let (resolved_mode, mode_conflict) = merge_file_modes(None, ours_mode, theirs_mode);
        let favor = merge_favor_for_path(options, &conflict.dest);
        let result = if is_mergeable_file_mode(ours_mode) && is_mergeable_file_mode(theirs_mode) {
            merge_blobs(
                &[],
                &ours_bytes,
                &theirs_bytes,
                &MergeBlobOptions {
                    ours_label: &qualify_label(options.ours_label, &conflict.ours_label_path),
                    theirs_label: &qualify_label(options.theirs_label, &conflict.theirs_label_path),
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor,
                    ws_ignore: options.ws_ignore,
                    marker_size: merge_marker_size_for_path(options, &conflict.dest),
                },
            )
        } else {
            MergeBlobResult {
                content: ours_bytes.clone(),
                conflicted: true,
            }
        };
        let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content.clone()))?;
        leaves.insert(conflict.dest.clone(), (resolved_mode, oid));
        slot.stages = MergeStages {
            base: None,
            ours: ours_entry,
            theirs: theirs_entry,
        };
        slot.result = Some((resolved_mode, oid));
        slot.worktree = Some((
            if ours_mode == theirs_mode {
                ours_mode
            } else {
                0o100644
            },
            result.content,
        ));
        slot.conflict = Some(MergeConflictKind::RenameRenameTwoToOne {
            ours_path: conflict.ours_source.clone(),
            theirs_path: conflict.theirs_source.clone(),
        });
        slot.auto_merged = !mode_conflict;
    }
    Ok(())
}

/// 3-way merge one rename's content into a single leaf entry: `base` is the
/// source's ancestor blob, `ours`/`theirs` the two sides' content (one of which
/// is the renamed file, the other the other side's change to the source). Both
/// present and differing → a real content merge; otherwise the surviving side's
/// entry is carried as-is.
fn rename_merged_leaf(
    db: &FileObjectDatabase,
    base: Option<(u32, ObjectId)>,
    ours: Option<(u32, ObjectId)>,
    theirs: Option<(u32, ObjectId)>,
    path: &[u8],
    options: &MergeTreesOptions<'_>,
) -> Result<Option<(u32, ObjectId)>> {
    match (ours, theirs) {
        (None, None) => Ok(None),
        (Some(entry), None) | (None, Some(entry)) => Ok(Some(entry)),
        (Some((ours_mode, ours_oid)), Some((theirs_mode, theirs_oid))) => {
            if (ours_mode, ours_oid) == (theirs_mode, theirs_oid) {
                return Ok(Some((ours_mode, ours_oid)));
            }
            if !is_mergeable_file_mode(ours_mode) || !is_mergeable_file_mode(theirs_mode) {
                return Ok(Some((ours_mode, ours_oid)));
            }
            let base_bytes = match base {
                Some((_, oid)) => merge_blob_bytes(db, &oid)?,
                None => Vec::new(),
            };
            let favor = merge_favor_for_path(options, path);
            let result = merge_blobs(
                &base_bytes,
                &merge_blob_bytes(db, &ours_oid)?,
                &merge_blob_bytes(db, &theirs_oid)?,
                &MergeBlobOptions {
                    ours_label: options.ours_label,
                    theirs_label: options.theirs_label,
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor,
                    ws_ignore: options.ws_ignore,
                    marker_size: merge_marker_size_for_path(options, path),
                },
            );
            let (mode, _) = merge_file_modes(base.map(|(mode, _)| mode), ours_mode, theirs_mode);
            let oid = db.write_object(EncodedObject::new(ObjectType::Blob, result.content))?;
            Ok(Some((mode, oid)))
        }
    }
}

/// Apply rename/rename(2to1) and rename/add conflicts: two distinct contents
/// land on one destination path. Each side's content at the destination is the
/// 3-way merge of its own rename (so the other side's change to the renamed
/// source follows the rename); the two results become stages 2 and 3 with no
/// common ancestor, and the worktree holds their two-way merge. The rename
/// source paths are consumed (removed from the path set) so they don't surface as
/// a spurious modify/delete.
#[allow(clippy::too_many_arguments)]
fn apply_rename_two_to_one_and_add_conflicts(
    db: &FileObjectDatabase,
    base_map: &MergeEntryMap,
    ours_map: &MergeEntryMap,
    theirs_map: &MergeEntryMap,
    renames: &MergeRenames,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    let mut consumed_sources: Vec<Vec<u8>> = Vec::new();

    for (dest, conflict) in &renames.rename_rename_two_to_one {
        // Ours renamed `ours_source`->dest; theirs' change to `ours_source`
        // follows the rename. Symmetric for theirs.
        let ours_leaf = rename_merged_leaf(
            db,
            base_map.get(&conflict.ours_source).copied(),
            ours_map.get(dest).copied(),
            theirs_map.get(&conflict.ours_source).copied(),
            dest,
            options,
        )?;
        let theirs_leaf = rename_merged_leaf(
            db,
            base_map.get(&conflict.theirs_source).copied(),
            ours_map.get(&conflict.theirs_source).copied(),
            theirs_map.get(dest).copied(),
            dest,
            options,
        )?;
        write_two_sided_dest_conflict(
            db,
            dest,
            ours_leaf,
            theirs_leaf,
            MergeConflictKind::RenameRenameTwoToOne {
                ours_path: conflict.ours_source.clone(),
                theirs_path: conflict.theirs_source.clone(),
            },
            options,
            paths,
            leaves,
        )?;
        consumed_sources.push(conflict.ours_source.clone());
        consumed_sources.push(conflict.theirs_source.clone());
    }

    for (dest, add) in &renames.rename_adds {
        let (ours_leaf, theirs_leaf) = match add.side {
            RenameSide::Ours => (
                rename_merged_leaf(
                    db,
                    base_map.get(&add.source).copied(),
                    ours_map.get(dest).copied(),
                    theirs_map.get(&add.source).copied(),
                    dest,
                    options,
                )?,
                theirs_map.get(dest).copied(),
            ),
            RenameSide::Theirs => (
                ours_map.get(dest).copied(),
                rename_merged_leaf(
                    db,
                    base_map.get(&add.source).copied(),
                    ours_map.get(&add.source).copied(),
                    theirs_map.get(dest).copied(),
                    dest,
                    options,
                )?,
            ),
        };
        write_two_sided_dest_conflict(
            db,
            dest,
            ours_leaf,
            theirs_leaf,
            MergeConflictKind::Content { add_add: true },
            options,
            paths,
            leaves,
        )?;
        consumed_sources.push(add.source.clone());
    }

    // The rename source paths are consumed by the rename: the other side's
    // change to them followed the rename to the destination, so they resolve to
    // a clean deletion (not the path-keyed core's modify/delete). Marking them
    // `Resolved(None)` lets the worktree writer remove the now-stale source file
    // rather than leaving it as a stray untracked file.
    for source in &consumed_sources {
        leaves.remove(source);
        if let Some(slot) = paths.iter_mut().find(|path| &path.path == source) {
            slot.stages = MergeStages::default();
            slot.result = None;
            slot.worktree = None;
            slot.conflict = None;
            slot.auto_merged = false;
        } else {
            paths.push(MergedPath {
                path: source.clone(),
                stages: MergeStages::default(),
                result: None,
                worktree: None,
                conflict: None,
                auto_merged: false,
            });
        }
    }
    Ok(())
}

/// Record a destination path that holds two unmerged contents (rename/rename
/// 2to1 or rename/add): stage 2 = `ours_leaf`, stage 3 = `theirs_leaf`, no
/// common ancestor, worktree = their two-way merge. Replaces any existing slot
/// (the path-keyed core's add/add result) for the destination.
#[allow(clippy::too_many_arguments)]
fn write_two_sided_dest_conflict(
    db: &FileObjectDatabase,
    dest: &[u8],
    ours_leaf: Option<(u32, ObjectId)>,
    theirs_leaf: Option<(u32, ObjectId)>,
    kind: MergeConflictKind,
    options: &MergeTreesOptions<'_>,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
) -> Result<()> {
    let ours_bytes = match ours_leaf {
        Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
        None => None,
    };
    let theirs_bytes = match theirs_leaf {
        Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
        None => None,
    };
    let (worktree_mode, worktree_content, result_leaf) = match (&ours_bytes, &theirs_bytes) {
        (Some((ours_mode, ours_content)), Some((theirs_mode, theirs_content))) => {
            let favor = merge_favor_for_path(options, dest);
            let merged = merge_blobs(
                &[],
                ours_content,
                theirs_content,
                &MergeBlobOptions {
                    ours_label: options.ours_label,
                    theirs_label: options.theirs_label,
                    base_label: options.ancestor_label,
                    style: options.style,
                    favor,
                    ws_ignore: options.ws_ignore,
                    marker_size: merge_marker_size_for_path(options, dest),
                },
            );
            let mode = if ours_mode == theirs_mode {
                *ours_mode
            } else {
                0o100644
            };
            let oid =
                db.write_object(EncodedObject::new(ObjectType::Blob, merged.content.clone()))?;
            (mode, merged.content, Some((mode, oid)))
        }
        (Some((mode, content)), None) | (None, Some((mode, content))) => {
            (*mode, content.clone(), ours_leaf.or(theirs_leaf))
        }
        (None, None) => (0o100644, Vec::new(), None),
    };

    let slot = MergedPath {
        path: dest.to_vec(),
        stages: MergeStages {
            base: None,
            ours: ours_leaf,
            theirs: theirs_leaf,
        },
        result: result_leaf,
        worktree: Some((worktree_mode, worktree_content)),
        conflict: Some(kind),
        auto_merged: true,
    };
    if let Some(existing) = paths.iter_mut().find(|path| path.path == dest) {
        *existing = slot;
    } else {
        paths.push(slot);
    }
    if let Some(leaf) = result_leaf {
        leaves.insert(dest.to_vec(), leaf);
    } else {
        leaves.remove(dest);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_rename_rename_one_to_two_conflicts(
    db: &FileObjectDatabase,
    base_map: &MergeEntryMap,
    eff_ours: &MergeEntryMap,
    eff_theirs: &MergeEntryMap,
    conflicts: &BTreeMap<Vec<u8>, RenameRenameOneToTwo>,
    paths: &mut Vec<MergedPath>,
    leaves: &mut MergeEntryMap,
    options: &MergeTreesOptions<'_>,
) -> Result<()> {
    for (old_path, conflict) in conflicts {
        let base_entry = base_map.get(old_path).copied();
        let ours_entry = eff_ours.get(&conflict.ours_dest).copied();
        let theirs_entry = eff_theirs.get(&conflict.theirs_dest).copied();
        let theirs_add_at_ours_dest = eff_theirs.get(&conflict.ours_dest).copied();
        let ours_add_at_theirs_dest = eff_ours.get(&conflict.theirs_dest).copied();

        leaves.remove(old_path);
        leaves.remove(&conflict.ours_dest);
        leaves.remove(&conflict.theirs_dest);
        paths.retain(|path| {
            path.path != *old_path
                && path.path != conflict.ours_dest
                && path.path != conflict.theirs_dest
        });

        paths.push(MergedPath {
            path: old_path.clone(),
            stages: MergeStages {
                base: base_entry,
                ours: None,
                theirs: None,
            },
            result: None,
            worktree: None,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwo {
                old_path: old_path.clone(),
                ours_path: conflict.ours_dest.clone(),
                theirs_path: conflict.theirs_dest.clone(),
                ours_label: options.ours_label.to_string(),
                theirs_label: options.theirs_label.to_string(),
            }),
            auto_merged: false,
        });

        let ours_worktree = match ours_entry {
            Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
            None => None,
        };
        paths.push(MergedPath {
            path: conflict.ours_dest.clone(),
            stages: MergeStages {
                base: None,
                ours: ours_entry,
                theirs: theirs_add_at_ours_dest,
            },
            result: None,
            worktree: ours_worktree,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwoStage),
            auto_merged: false,
        });

        let theirs_worktree = match theirs_entry {
            Some((mode, oid)) => Some((mode, merge_worktree_bytes(db, mode, &oid)?)),
            None => None,
        };
        paths.push(MergedPath {
            path: conflict.theirs_dest.clone(),
            stages: MergeStages {
                base: None,
                ours: ours_add_at_theirs_dest,
                theirs: theirs_entry,
            },
            result: None,
            worktree: theirs_worktree,
            conflict: Some(MergeConflictKind::RenameRenameOneToTwoStage),
            auto_merged: false,
        });
    }
    Ok(())
}

/// Build a path-qualified conflict-marker label `"<label>:<path>"`, as git does
/// for renamed files (so the two sides of a conflict name their distinct paths).
fn qualify_label(label: &str, path: &[u8]) -> String {
    format!("{label}:{}", String::from_utf8_lossy(path))
}

/// Adapt a flat `path -> (mode, oid)` map into the `TrackedEntry` map the
/// name-status diff core consumes.
fn entry_map_as_tracked(map: &MergeEntryMap) -> BTreeMap<Vec<u8>, TrackedEntry> {
    map.iter()
        .map(|(path, (mode, oid))| {
            (
                path.clone(),
                TrackedEntry {
                    mode: *mode,
                    oid: *oid,
                },
            )
        })
        .collect()
}
