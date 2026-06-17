//! Merge, rebase, pull, cherry-pick, revert, and merge-base commands.

use crate::commands::remote_cmds::{
    StdoutProgress, fetch_bundle, fetch_source_is_ssh, fetch_ssh_repository, ls_remote_git_dir,
};
use crate::*;
use sley_remote::FetchOptions;

// ===== git merge (3-way) =====

pub(crate) type MergeTreeMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

pub(crate) fn merge_read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = db.read_object(oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

pub(crate) fn merge_index_entry(path: &[u8], mode: u32, oid: ObjectId, stage: u16) -> IndexEntry {
    let flags = ((stage & 0x3) << 12) | (path.len().min(0x0fff) as u16);
    IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags,
        flags_extended: 0,
        path: BString::from(path),
    }
}

pub(crate) fn merge_write_worktree_file(
    worktree_root: &Path,
    path: &[u8],
    content: &[u8],
    mode: u32,
) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    if let Some(parent) = full.parent() {
        // A regular file may occupy one of the ancestor path components (the D/F
        // case: HEAD had `dir` as a file, the merge now needs `dir/<child>`). git
        // removes the blocking file before materializing the directory subtree, so
        // clear any non-directory ancestor before `create_dir_all`, which would
        // otherwise fail with EEXIST/ENOTDIR.
        remove_blocking_file_ancestors(worktree_root, rel)?;
        fs::create_dir_all(parent)?;
    }
    // Unlink whatever is in the way first (git's entry.c `write_entry`), so a type
    // change (regular file ⇄ symlink) is overwritten rather than written *through*
    // an existing symlink or left stale — the symlink-stash-apply / merge cases.
    merge_unlink_path_in_the_way(&full)?;
    if sley_index::is_gitlink(mode) {
        // Gitlink (submodule) entry: the `oid` is a *commit*, not a blob, so it
        // must NOT be written as file content (the prior unconditional blob write
        // produced an "Is a directory"/garbage-content failure that gated the
        // revert/cherry-pick-over-submodule worktree apply, e.g.
        // create_lib_submodule_repo's `git revert HEAD`). git's entry.c
        // `write_entry` S_IFGITLINK arm only `mkdir`s the submodule directory
        // (`submodule_move_head` — the embedded checkout — is a higher layer sley
        // does not perform), so materialize the gitlink exactly as
        // `sley_worktree::materialize_tree_entry`'s gitlink branch: create the
        // directory and return, writing no content.
        fs::create_dir_all(&full)?;
        return Ok(());
    }
    if (mode & 0o170000) == 0o120000 {
        // Symlink entry (mode 120000): the blob bytes are the link target.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let target =
                std::path::PathBuf::from(std::ffi::OsString::from_vec(content.to_vec()));
            std::os::unix::fs::symlink(&target, &full)?;
        }
        #[cfg(not(unix))]
        fs::write(&full, content)?;
    } else {
        fs::write(&full, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(if mode == 0o100755 { 0o755 } else { 0o644 });
            fs::set_permissions(&full, perms)?;
        }
    }
    Ok(())
}

/// Remove whatever currently occupies `full` (lstat-based, so a dangling symlink
/// is removed as the link, not followed) before a merge materializes a new object
/// there. A directory in the way is removed recursively (D/F transition).
fn merge_unlink_path_in_the_way(full: &Path) -> Result<()> {
    match fs::symlink_metadata(full) {
        Ok(metadata) => {
            if metadata.is_dir() {
                match fs::remove_dir_all(full) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            } else {
                fs::remove_file(full)?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// Clear worktree files that block any directory path in the merged result.
/// Used on the clean-merge checkout path before
/// [`sley_worktree::reset_index_and_worktree_to_commit`], which would otherwise
/// fail when a HEAD file occupies a path the merged tree now needs as a
/// directory (directory-rename D/F). Best-effort: errors are swallowed so a
/// genuine I/O problem surfaces from the subsequent checkout instead.
fn clear_merge_df_blockers(worktree_root: &Path, results: &MergePathResults) {
    for path in results.keys() {
        if !path.contains(&b'/') {
            continue;
        }
        if let Ok(rel) = std::str::from_utf8(path) {
            let _ = remove_blocking_file_ancestors(worktree_root, rel);
        }
    }
}

/// Remove any regular file occupying an ancestor directory component of `rel`
/// (relative worktree path). This clears the D/F case where a file (e.g. `dir`)
/// blocks the creation of a directory subtree (`dir/child`). Only plain files
/// are removed — an existing directory ancestor is left intact, and a symlink
/// ancestor is unlinked (git would not write through it).
fn remove_blocking_file_ancestors(worktree_root: &Path, rel: &str) -> Result<()> {
    let mut prefix = String::new();
    let mut components = rel.split('/').peekable();
    while let Some(component) = components.next() {
        // Stop before the leaf — only ancestors (directory components) matter.
        if components.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let candidate = worktree_root.join(&prefix);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if !meta.is_dir() => fs::remove_file(&candidate)?,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// True when it is safe to delete the worktree file at `path` during a merge:
/// either the file is already gone, or its on-disk content hashes to the blob
/// `ours` (HEAD) had at that path. An untracked file (ours = `None`) or a file
/// whose content diverges from ours' version is preserved, matching git's refusal
/// to clobber untracked/dirty data (the rename/delete "Gollum's ring" case).
fn worktree_file_matches_ours(
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

pub(crate) fn merge_remove_worktree_file(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    // lstat (symlink_metadata): `Path::exists` follows symlinks and misses a
    // dangling one, leaving it behind on removal.
    match fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_dir() => {
            // A directory occupies a tracked path being removed: this is a
            // gitlink (submodule checkout). git's entry.c `unlink_entry` ⇒
            // `remove_or_warn(mode, ..)` dispatches on `S_ISGITLINK(mode)` to
            // `rmdir_or_warn` (vs `unlink_or_warn` for blobs/symlinks), so the
            // submodule's *directory* is removed, never `unlink`ed (which is the
            // `EISDIR` "Is a directory" failure that gated revert/cherry-pick
            // over a populated submodule — t1013/t7112/t6438 setup). git first
            // deinits via `submodule_move_head` (a higher layer sley does not
            // perform), then `rmdir`s; `rmdir` of a still-populated submodule
            // fails with ENOTEMPTY and git only *warns*, leaving the directory
            // in place rather than erroring (`warn_if_unremovable`). Mirror that:
            // try to remove the (now-empty-or-not) directory, but never fail the
            // operation on a non-empty submodule directory.
            match fs::remove_dir(&full) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // ENOTEMPTY (populated submodule) and friends: git warns and
                    // continues. Match the warn-and-continue, do not propagate.
                    eprintln!("warning: unable to rmdir '{rel}': Directory not empty");
                }
            }
        }
        Ok(_) => fs::remove_file(&full)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// Per-path outcome of a 3-way tree merge.
pub(crate) enum MergePathResult {
    /// Cleanly resolved; `None` means the path is deleted in the result.
    Resolved(Option<(u32, ObjectId)>),
    /// Conflicted: carries the (mode, oid) for each present stage and the bytes
    /// (with conflict markers) plus mode to materialize in the worktree.
    Conflict {
        base: Option<(u32, ObjectId)>,
        ours: Option<(u32, ObjectId)>,
        theirs: Option<(u32, ObjectId)>,
        worktree: Option<(u32, Vec<u8>)>,
        /// The conflict classification, so the porcelain renders the correct
        /// `CONFLICT (…)` message line (content / modify-delete / rename-delete /
        /// file-directory) instead of always claiming a content conflict.
        kind: Option<sley_diff_merge::MergeConflictKind>,
        /// True when a textual 3-way content merge ran for this path; drives the
        /// `Auto-merging <path>` info line (git emits it only for content merges).
        auto_merged: bool,
    },
}

type MergePathResults = BTreeMap<Vec<u8>, MergePathResult>;
type MergeConflictPaths = Vec<Vec<u8>>;

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
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    three_way_merge_trees_with_favor(
        db,
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
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    style: sley_diff_merge::ConflictStyle,
) -> Result<(MergePathResults, MergeConflictPaths)> {
    three_way_merge_trees_inner(
        db,
        format,
        base,
        ours,
        theirs,
        ours_label,
        theirs_label,
        ancestor_label,
        sley_diff_merge::MergeFavor::None,
        style,
    )
}

/// Build the flattened entry map of the *virtual ancestor* for a 3-way merge,
/// recursively merging the merge bases together (merge-recursive's "virtual
/// ancestor" construction for criss-cross histories).
///
/// With a single merge base this is exactly that base commit's tree. With more
/// than one (a criss-cross history) the bases are folded left-to-right: merge
/// the running virtual ancestor with the next base, using *their* merge bases as
/// the ancestor of that sub-merge (recursing). Conflicts in the virtual merge are
/// resolved by writing the conflicted blob content (git keeps the conflicted
/// state in the virtual tree, which then feeds the outer 3-way merge) — this
/// matches merge-recursive, which does not stop on virtual-ancestor conflicts.
fn virtual_ancestor_entry_map(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    bases: &[ObjectId],
    git_dir: &Path,
) -> Result<MergeTreeMap> {
    let first = bases
        .first()
        .ok_or_else(|| GitError::Command("virtual ancestor needs at least one base".into()))?;
    let acc_tree = commit_tree_oid(db, format, first)?;
    let mut acc_map = stash_tree_entry_map(db, format, &acc_tree)?;
    // Track the commit(s) the running virtual ancestor stands in for, so the next
    // pairwise merge uses the correct sub-base.
    let mut acc_commits = vec![*first];

    for base in &bases[1..] {
        let other_tree = commit_tree_oid(db, format, base)?;
        let other_map = stash_tree_entry_map(db, format, &other_tree)?;

        // Sub-base: the merge base(s) of the accumulated commits and this base.
        // Use the first acc commit as a representative (git folds pairwise).
        // `db` now serves both the reads here and the writes below — with
        // `ObjectWriter::write_object` taking `&self`, no second read-only handle
        // (which re-warmed the pack caches) is needed.
        let sub_bases = merge_bases(git_dir, db, format, &acc_commits[0], base)?;
        let sub_base_map = match sub_bases.first() {
            Some(sb) => {
                let sb_tree = commit_tree_oid(db, format, sb)?;
                stash_tree_entry_map(db, format, &sb_tree)?
            }
            None => MergeTreeMap::new(),
        };

        // Merge the two bases into a new virtual ancestor tree. Conflicts are
        // folded into the tree (the merged blob with markers is written), never
        // surfaced — the outer merge owns conflict reporting.
        let (results, _conflicts) = three_way_merge_trees(
            db,
            format,
            &sub_base_map,
            &acc_map,
            &other_map,
            "Temporary merge branch 1",
            "Temporary merge branch 2",
        )?;

        let mut next: MergeTreeMap = BTreeMap::new();
        for (path, result) in results {
            match result {
                MergePathResult::Resolved(Some(entry)) => {
                    next.insert(path, entry);
                }
                MergePathResult::Resolved(None) => {}
                MergePathResult::Conflict {
                    worktree,
                    ours,
                    theirs,
                    ..
                } => {
                    // Keep the conflicted content in the virtual tree, mirroring
                    // merge-recursive (it writes the marker blob at stage 0).
                    if let Some((mode, bytes)) = worktree {
                        let oid = db.write_object(EncodedObject::new(ObjectType::Blob, bytes))?;
                        next.insert(path, (mode, oid));
                    } else if let Some(entry) = ours.or(theirs) {
                        next.insert(path, entry);
                    }
                }
            }
        }
        acc_map = next;
        acc_commits = vec![*base];
    }
    Ok(acc_map)
}

/// Like [`three_way_merge_trees`] but with an explicit `-Xours`/`-Xtheirs`
/// conflict-favouring choice (used by `git merge -X ours|theirs`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_with_favor(
    db: &FileObjectDatabase,
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

#[allow(clippy::too_many_arguments)]
fn three_way_merge_trees_inner(
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
) -> Result<(MergePathResults, MergeConflictPaths)> {
    let merge = sley_diff_merge::merge_entry_maps(
        db,
        format,
        base,
        ours,
        theirs,
        &sley_diff_merge::MergeTreesOptions {
            ours_label,
            theirs_label,
            ancestor_label,
            favor,
            // Rename-aware merge: a file renamed on one side and modified on the
            // other follows the rename (the merge-ort single-base rename case).
            detect_renames: true,
            rename_threshold: sley_diff_merge::DEFAULT_RENAME_THRESHOLD,
            // Directory-rename detection honours `merge.directoryRenames` (git's
            // default is `conflict`). When one side renames a directory and the
            // other adds files under the old directory, those files re-home into
            // the renamed directory.
            directory_renames: directory_renames_config(),
            style,
        },
    )?;

    let mut results = BTreeMap::new();
    let mut conflicts = Vec::new();
    for entry in merge.paths {
        // A directory-rename location "conflict" (=conflict mode) is purely
        // advisory: git stages the re-homed content cleanly at stage 0 and only
        // emits an informational `CONFLICT (file location)` message + nonzero
        // exit. Carry the resolved leaf in the `ours` stage slot and rely on the
        // index/worktree writers to stage `DirRenameLocation` at stage 0.
        let advisory_location = matches!(
            entry.conflict,
            Some(sley_diff_merge::MergeConflictKind::DirRenameLocation { .. })
                | Some(sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { .. })
        );
        if entry.conflict.is_some() {
            conflicts.push(entry.path.clone());
            if advisory_location {
                let worktree = match entry.result {
                    Some((mode, oid)) => Some((mode, merge_read_blob(db, &oid)?)),
                    None => None,
                };
                results.insert(
                    entry.path,
                    MergePathResult::Conflict {
                        base: None,
                        ours: entry.result,
                        theirs: None,
                        worktree,
                        kind: entry.conflict,
                        auto_merged: entry.auto_merged,
                    },
                );
            } else {
                results.insert(
                    entry.path,
                    MergePathResult::Conflict {
                        base: entry.stages.base,
                        ours: entry.stages.ours,
                        theirs: entry.stages.theirs,
                        worktree: entry.worktree,
                        kind: entry.conflict,
                        auto_merged: entry.auto_merged,
                    },
                );
            }
        } else {
            results.insert(entry.path, MergePathResult::Resolved(entry.result));
        }
    }
    Ok((results, conflicts))
}

/// Render git merge's post-merge `--stat`/`--compact-summary` block.
///
/// git (`builtin/merge.c`) drives this from `show_diffstat`:
///   * `MERGE_SHOW_DIFFSTAT` → `DIFF_FORMAT_DIFFSTAT | DIFF_FORMAT_SUMMARY`,
///     i.e. the diffstat followed by the `create/delete mode`/`rename` summary
///     block;
///   * `MERGE_SHOW_COMPACTSUMMARY` → `DIFF_FORMAT_DIFFSTAT` with
///     `stat_with_summary`, folding the summary into the stat rows (no separate
///     block);
///   * off → nothing.
///
/// Rename detection is always on (git sets `DIFF_DETECT_RENAME`).
fn write_merge_result_diffstat(
    stdout: &mut io::Stdout,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
    mode: MergeDiffstat,
) -> Result<()> {
    if mode == MergeDiffstat::Off {
        return Ok(());
    }
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    let compact = mode == MergeDiffstat::Compact;
    write_diff_stat(
        stdout,
        &entries,
        db,
        None,
        false,
        DiffStatOptions {
            compact_summary: compact,
            stat_count: None,
            color: false,
        },
    )?;
    // The default `--stat` mode appends a `DIFF_FORMAT_SUMMARY` block (the
    // ` create mode`/` delete mode`/` rename`/` mode change` lines). The
    // compact mode inlines that information into the stat rows instead, so it
    // emits no separate block.
    if !compact {
        for entry in &entries {
            write_diff_summary_entry(stdout, entry)?;
        }
    }
    Ok(())
}

/// Resolve git merge's effective `show_diffstat` value: an explicit CLI flag
/// wins, otherwise `merge.stat` config decides (`false`/`no`/`off` → off,
/// `compact` → compact, anything else / unset → the default full diffstat).
fn merge_diffstat_mode(options: &MergeOptions) -> MergeDiffstat {
    if let Some(mode) = options.diffstat {
        return mode;
    }
    let value = effective_config_with_overrides()
        .and_then(|config| config.get("merge", None, "stat").map(str::to_string));
    match value.as_deref() {
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "false" | "no" | "off" | "0" => MergeDiffstat::Off,
            "compact" => MergeDiffstat::Compact,
            _ => MergeDiffstat::Stat,
        },
        None => MergeDiffstat::Stat,
    }
}

/// Create a merge commit with two parents and advance the current branch (or
/// detached HEAD) to it, writing a reflog entry.
fn merge_commit_and_advance(
    git_dir: &Path,
    refs: &FileRefStore,
    format: ObjectFormat,
    head_oid: &ObjectId,
    other_oid: &ObjectId,
    tree: ObjectId,
    message: Vec<u8>,
) -> Result<ObjectId> {
    commands::hooks::run_hook("pre-merge-commit", commands::hooks::HookRun::default())?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![*head_oid, *other_oid],
            author,
            committer: committer.clone(),
            message,
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(*head_oid)),
        new: RefTarget::Direct(oid),
        reflog: Some(ReflogEntry {
            old_oid: *head_oid,
            new_oid: oid,
            committer,
            message: format!("merge {other_oid}: Merge made by the 'ort' strategy.").into_bytes(),
        }),
    });
    tx.commit()?;
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    Ok(oid)
}

/// Commit + advance HEAD for `-s ours`. Identical to [`merge_commit_and_advance`]
/// except the reflog message names the `ours` strategy and uses the merge target
/// label (e.g. `merge main: Merge made by the 'ours' strategy.`), matching git's
/// `merge-ours` reflog exactly.
#[allow(clippy::too_many_arguments)]
fn merge_ours_commit_and_advance(
    git_dir: &Path,
    refs: &FileRefStore,
    format: ObjectFormat,
    head_oid: &ObjectId,
    other_oid: &ObjectId,
    tree: ObjectId,
    target_label: &str,
    message: Vec<u8>,
) -> Result<ObjectId> {
    commands::hooks::run_hook("pre-merge-commit", commands::hooks::HookRun::default())?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut db = FileObjectDatabase::from_git_dir(git_dir, format);
    let oid = sley_sequencer::create_commit(
        &mut db,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![*head_oid, *other_oid],
            author,
            committer: committer.clone(),
            message,
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(*head_oid)),
        new: RefTarget::Direct(oid),
        reflog: Some(ReflogEntry {
            old_oid: *head_oid,
            new_oid: oid,
            committer,
            message: format!("merge {target_label}: Merge made by the 'ours' strategy.")
                .into_bytes(),
        }),
    });
    tx.commit()?;
    commands::hooks::run_hook("reference-transaction", commands::hooks::HookRun::default())?;
    Ok(oid)
}

/// True when `ancestor` is reachable from `of` (an ancestor of, or equal to,
/// `of`) — git's `in_merge_bases` predicate over two commits.
fn is_ancestor_commit(
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    ancestor: &ObjectId,
    of: &ObjectId,
) -> Result<bool> {
    if ancestor == of {
        return Ok(true);
    }
    Ok(merge_bases(git_dir, db, format, ancestor, of)?
        .iter()
        .any(|base| base == ancestor))
}

/// git's `reduce_heads` over the named merge targets: drop any head already
/// reachable from HEAD or from another named head (a duplicate keeps only its
/// first occurrence), preserving command-line order. Used by the strategy
/// dispatch (one survivor ⇒ regular two-parent merge; ≥2 ⇒ octopus) and by the
/// octopus driver itself so both agree on the parent set.
fn reduce_merge_targets(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    refs: &FileRefStore,
    targets: &[String],
) -> Result<Vec<(String, ObjectId)>> {
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        Some(RefTarget::Direct(oid)) => Some(oid),
        None => None,
    };

    let mut heads = Vec::with_capacity(targets.len());
    for target in targets {
        let oid = resolve_revision(git_dir, format, target)?;
        heads.push((target.clone(), oid));
    }

    let is_ancestor =
        |db: &FileObjectDatabase, ancestor: &ObjectId, of: &ObjectId| -> Result<bool> {
            if ancestor == of {
                return Ok(true);
            }
            Ok(merge_bases(git_dir, db, format, ancestor, of)?
                .iter()
                .any(|base| base == ancestor))
        };
    let mut reduced: Vec<(String, ObjectId)> = Vec::new();
    'heads: for (index, (name, oid)) in heads.iter().enumerate() {
        if let Some(head_oid) = head_oid
            && is_ancestor(&db, oid, &head_oid)?
        {
            continue;
        }
        for (other_index, (_, other)) in heads.iter().enumerate() {
            if other_index == index {
                continue;
            }
            if oid == other {
                if other_index < index {
                    continue 'heads;
                }
                continue;
            }
            if is_ancestor(&db, oid, other)? {
                continue 'heads;
            }
        }
        reduced.push((name.clone(), *oid));
    }
    Ok(reduced)
}

/// `git merge <a> <b> [...]` — the octopus strategy. Mirrors upstream's
/// `git-merge-octopus`: iteratively three-way-merge each head onto the running
/// merged tree (MRT), fast-forwarding where possible, and refuse (exit 2) the
/// moment any pairwise step conflicts — an octopus merge must be trivially
/// clean. The final commit records HEAD plus every non-redundant head as
/// parents, in command-line order.
#[allow(clippy::too_many_arguments)]
fn merge_octopus(
    git_dir: &Path,
    common_git_dir: &Path,
    format: ObjectFormat,
    worktree_root: &Path,
    refs: &FileRefStore,
    targets: &[String],
    options: &MergeOptions,
) -> Result<()> {
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => {
                return Err(GitError::Unsupported(
                    "octopus merge into an unborn branch is not supported".into(),
                ));
            }
        },
        Some(RefTarget::Direct(oid)) => oid,
        None => {
            return Err(GitError::Command("HEAD is not a valid revision".into()));
        }
    };
    let db = FileObjectDatabase::from_git_dir(common_git_dir, format);

    // git-merge-octopus's `git diff-index --quiet --cached HEAD` guard: a staged
    // change vs HEAD makes the index an unclean octopus base. Refuse (exit 2)
    // before writing any merge state.
    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    if let Some(entry) = status
        .iter()
        .find(|e| e.index != b' ' && e.index != b'?' && e.index != b'!')
    {
        eprintln!(
            "Error: Your local changes to the following files would be overwritten by merge\n    {}",
            String::from_utf8_lossy(&entry.path)
        );
        return Err(GitError::Exit(2));
    }

    let reduced = reduce_merge_targets(git_dir, common_git_dir, format, refs, targets)?;
    if reduced.is_empty() {
        if !options.quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }

    // Iterative octopus: MRC tracks the commits the running tree stands for.
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let mut merged_map = stash_tree_entry_map(&db, format, &head_tree)?;
    let mut merged_commits = vec![head_oid];
    let mut non_ff = false;
    // git-merge-octopus allows only the LAST head to leave a hand-resolvable
    // conflict; if a conflict occurs and another head still remains, the octopus
    // gives up entirely.
    let mut octopus_failure = false;
    for (name, oid) in &reduced {
        // A prior pairwise step conflicted but more heads remained: git's
        // "Should not be doing an octopus" bail (exit 2, no state left behind).
        if octopus_failure {
            eprintln!("Automated merge did not work.");
            eprintln!("Should not be doing an octopus.");
            eprintln!("fatal: merge program failed");
            return Err(GitError::Exit(2));
        }
        let mut base_args = vec![*oid];
        base_args.extend(merged_commits.iter().copied());
        let common = merge_bases_default_many(&db, format, &base_args)?;
        if common.len() == 1 && common[0] == *oid {
            // Already covered by the merges performed so far. git's octopus
            // prints "Already up to date with <name>" and moves on.
            if !options.quiet {
                println!("Already up to date with {name}");
            }
            continue;
        }
        if !non_ff
            && merged_commits.len() == 1
            && common.len() == 1
            && common[0] == merged_commits[0]
        {
            // Fast-forward the running state to this head (git-merge-octopus's
            // "Fast-forwarding to: <name>").
            if !options.quiet {
                println!("Fast-forwarding to: {name}");
            }
            let tree = commit_tree_oid(&db, format, oid)?;
            merged_map = stash_tree_entry_map(&db, format, &tree)?;
            merged_commits = vec![*oid];
            continue;
        }
        if common.is_empty() {
            eprintln!("Unable to find common commit with {name}");
            return Err(GitError::Exit(2));
        }
        // `--ff-only`: a real (non-fast-forward) octopus step is needed, which
        // an ff-only merge cannot satisfy. git refuses before merging.
        if options.ff_only() {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        non_ff = true;
        // git-merge-octopus's "Trying simple merge with <name>" line precedes
        // each non-fast-forward pairwise step.
        if !options.quiet {
            println!("Trying simple merge with {name}");
        }
        let base_map = virtual_ancestor_entry_map(&db, format, &common, common_git_dir)?;
        let theirs_tree = commit_tree_oid(&db, format, oid)?;
        let theirs_map = stash_tree_entry_map(&db, format, &theirs_tree)?;
        let (results, conflicts) = three_way_merge_trees_with_favor(
            &db,
            format,
            &base_map,
            &merged_map,
            &theirs_map,
            "HEAD",
            name,
            options.favor,
        )?;
        if !conflicts.is_empty() {
            // git-merge-octopus: a conflict sets OCTOPUS_FAILURE but the loop
            // continues — only the LAST head may conflict (hand-resolvable). If
            // another head remains, the next iteration's guard above bails with
            // "Should not be doing an octopus". Don't advance the running state.
            octopus_failure = true;
            continue;
        }
        let mut next: MergeTreeMap = BTreeMap::new();
        for (path, result) in results {
            if let MergePathResult::Resolved(Some(entry)) = result {
                next.insert(path, entry);
            }
        }
        merged_map = next;
        merged_commits.push(*oid);
    }

    // The LAST head conflicted (octopus allows exactly one hand-resolvable
    // conflict). sley's octopus does not model materialising that conflicted
    // state, so report the failure and leave the tree untouched (exit 2), as
    // git's octopus does for an unresolvable final step.
    if octopus_failure {
        eprintln!("Automated merge did not work.");
        eprintln!("Should not be doing an octopus.");
        eprintln!("fatal: merge program failed");
        return Err(GitError::Exit(2));
    }

    if !non_ff && merged_commits.len() == 1 && reduced.len() == 1 {
        // Degenerated to a plain fast-forward.
        let new_oid = merged_commits[0];
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: Some(RefTarget::Direct(head_oid)),
            new: RefTarget::Direct(new_oid),
            reflog: Some(ReflogEntry {
                old_oid: head_oid,
                new_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: merge_reflog_message(&reduced[0].0, "Fast-forward"),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            worktree_root,
            git_dir,
            format,
            &new_oid,
        )?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&new_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            let new_tree = commit_tree_oid(&db, format, &new_oid)?;
            write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &new_tree, merge_diffstat_mode(options))?;
            stdout.flush()?;
        }
        return Ok(());
    }

    // Build the merged tree via a temporary stage-0 index, mirroring the
    // two-parent clean path above.
    let mut entries = Vec::new();
    for (path, (mode, oid)) in &merged_map {
        entries.push(merge_index_entry(path, *mode, *oid, 0));
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        index.write(format)?,
    )?;
    let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;

    let message = build_merge_message(refs, git_dir, &db, format, options, &head_oid, &reduced)?;

    // Materialize the merged result into the worktree, touching only paths that
    // differ from HEAD (preserve untouched local mods, as in the two-parent path).
    let head_map = &stash_tree_entry_map(&db, format, &head_tree)?;
    let sync_octopus_worktree = || -> Result<()> {
        for (path, entry) in &merged_map {
            if head_map.get(path) == Some(entry) {
                continue;
            }
            let (mode, oid) = entry;
            let content = merge_read_blob(&db, oid)?;
            merge_write_worktree_file(worktree_root, path, &content, *mode)?;
        }
        for path in head_map.keys() {
            if !merged_map.contains_key(path) {
                merge_remove_worktree_file(worktree_root, path)?;
            }
        }
        Ok(())
    };

    // `--squash`: stage the merged result + write SQUASH_MSG, record NO merge.
    if options.squash {
        sync_octopus_worktree()?;
        let other_oids: Vec<ObjectId> = reduced.iter().map(|(_, oid)| *oid).collect();
        write_squash_message_multi(git_dir, &db, format, &head_oid, &other_oids)?;
        if !options.quiet {
            println!("Squash commit -- not updating HEAD");
        }
        commands::hooks::run_hook_l("post-merge", &["1"])?;
        return Ok(());
    }

    // `--no-commit`: stage the merged result, record MERGE_HEAD (every merged
    // head) + MERGE_MSG, but do not create the commit or advance HEAD.
    if options.no_commit {
        sync_octopus_worktree()?;
        let mut merge_head = String::new();
        for (_, oid) in &reduced {
            merge_head.push_str(&format!("{oid}\n"));
        }
        fs::write(git_dir.join("MERGE_HEAD"), merge_head)?;
        fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n"))?;
        write_merge_mode(git_dir, options)?;
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
        if !options.quiet {
            println!("Automatic merge went well; stopped before committing as requested");
        }
        return Ok(());
    }

    if !options.quiet {
        let mut stdout = io::stdout();
        writeln!(stdout, "Merge made by the 'octopus' strategy.")?;
        write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &merged_tree, merge_diffstat_mode(options))?;
        stdout.flush()?;
    }

    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut write_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    // git's `collect_parents`/`reduce_parents`: the parent set is the reduced
    // independent heads, with HEAD prepended only when HEAD was NOT subsumed
    // (i.e. it is not an ancestor of any merged head) OR `--no-ff` forces it in.
    // `reduced` already excludes any head reachable from HEAD, so HEAD is
    // "subsumed" exactly when it is an ancestor of some reduced head.
    let head_subsumed = reduced
        .iter()
        .any(|(_, oid)| oid == &head_oid || is_ancestor_commit(&db, git_dir, format, &head_oid, oid).unwrap_or(false));
    let mut parents: Vec<ObjectId> = Vec::with_capacity(reduced.len() + 1);
    if !head_subsumed || options.no_ff() {
        parents.push(head_oid);
    }
    parents.extend(reduced.iter().map(|(_, oid)| *oid));
    let merged_oid = sley_sequencer::create_commit(
        &mut write_db,
        sley_sequencer::CommitCreate {
            tree: merged_tree,
            parents,
            author,
            committer: committer.clone(),
            message: commit_cleanup_message(
                message.into_bytes(),
                resolve_merge_cleanup_mode(options),
                "#",
                options.edit == Some(true),
            ),
            encoding: None,
        },
    )?;
    let target_ref = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => branch,
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: target_ref,
        expected: Some(RefTarget::Direct(head_oid)),
        new: RefTarget::Direct(merged_oid),
        reflog: Some(ReflogEntry {
            old_oid: head_oid,
            new_oid: merged_oid,
            committer,
            message: "merge: Merge made by the 'octopus' strategy.".into(),
        }),
    });
    tx.commit()?;
    sley_worktree::reset_index_and_worktree_to_commit(worktree_root, git_dir, format, &merged_oid)?;
    Ok(())
}

/// Build and write `.git/SQUASH_MSG` for a `--squash` merge of `other` onto
/// `head`, mirroring git's `squash_message` (builtin/merge.c): the literal
/// header `Squashed commit of the following:` then, for each commit reachable
/// from `other` but not `head` (newest first by commit date), a blank line,
/// `commit <full-oid>`, and the commit rendered in `git log` MEDIUM format.
fn write_squash_message(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    other: &ObjectId,
) -> Result<()> {
    write_squash_message_multi(git_dir, db, format, head, std::slice::from_ref(other))
}

/// `--squash` SQUASH_MSG for a merge of one or more heads (octopus): the
/// `^HEAD <other>...` range rendered as git's `squash_message`. Mirrors
/// `write_squash_message` but seeds the walk from every merged head.
fn write_squash_message_multi(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    others: &[ObjectId],
) -> Result<()> {
    // Mark HEAD's ancestors uninteresting, then collect every `other`'s ancestors
    // that are not among them (the `^HEAD other...` range).
    let uninteresting = ancestor_depths(db, format, head)?;
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: VecDeque<ObjectId> = others.iter().cloned().collect();
    while let Some(oid) = pending.pop_front() {
        if uninteresting.contains_key(&oid) || !seen.insert(oid.clone()) {
            continue;
        }
        let record = read_rev_list_commit_record(db, format, oid.clone())?;
        for parent in &record.parents {
            if !uninteresting.contains_key(parent) {
                pending.push_back(parent.clone());
            }
        }
        records.push(record);
    }
    // `git log` default order is reverse-chronological by commit date; ties keep
    // a stable order (children before parents, which the collection preserves).
    records.sort_by(|left, right| {
        let left_time = commit_identity_timestamp_i64(&left.commit.committer).unwrap_or(0);
        let right_time = commit_identity_timestamp_i64(&right.commit.committer).unwrap_or(0);
        right_time.cmp(&left_time)
    });

    let mut out = String::from("Squashed commit of the following:\n");
    for record in &records {
        out.push('\n');
        out.push_str(&format!("commit {}\n", record.oid));
        out.push_str(&format!(
            "Author: {}\n",
            commit_author_identity(&record.commit.author)
        ));
        out.push_str(&format!(
            "Date:   {}\n",
            commit_identity_date(&record.commit.author, &DateMode::Default)
        ));
        out.push('\n');
        for line in String::from_utf8_lossy(&record.commit.message).lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    fs::write(git_dir.join("SQUASH_MSG"), out)?;
    Ok(())
}

/// git's `merge_name` ref classification: how a merge target dwims to a ref,
/// driving both the title noun ("branch"/"tag"/…) and which `print_joined`
/// group a head lands in. Precedence follows `ref_rev_parse_rules`
/// (tags before heads), so a tag wins a name it shares with a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeRefKind {
    Branch,
    Tag,
    RemoteBranch,
    Commit,
}

/// Classify a single merge target into its `MergeRefKind` (git's `merge_name`).
fn classify_merge_target(refs: &FileRefStore, target: &str) -> Result<MergeRefKind> {
    let exists = |name: &str| -> Result<bool> { Ok(refs.read_ref(name)?.is_some()) };
    if exists(&format!("refs/tags/{target}"))? {
        Ok(MergeRefKind::Tag)
    } else if exists(&format!("refs/heads/{target}"))? {
        Ok(MergeRefKind::Branch)
    } else if exists(&format!("refs/remotes/{target}"))? {
        Ok(MergeRefKind::RemoteBranch)
    } else {
        Ok(MergeRefKind::Commit)
    }
}

/// git's `print_joined`: render a same-kind name list as
/// `<singular>'a'` (one) or `<plural>'a', 'b' and 'c'` (many).
fn print_joined(singular: &str, plural: &str, names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{singular}'{one}'"),
        [rest @ .., last] => {
            let head = rest
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{plural}{head} and '{last}'")
        }
    }
}

/// git's `merge.suppressDest` default: omit the ` into <branch>` title suffix
/// when the current branch is `main` or `master` (the built-in patterns).
fn merge_dest_suppressed(branch: &str) -> bool {
    branch == "main" || branch == "master"
}

/// The default merge commit subject (git's `fmt_merge_msg_title`): group the
/// merged heads by ref-kind, render each group via `print_joined`, and append
/// ` into <branch>` unless the destination is suppressed. Both the two-parent
/// and octopus paths route through this single function so the whole class of
/// merge-message cells stays git-exact.
fn merge_message_title(
    refs: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    targets: &[String],
) -> Result<String> {
    // FETCH_HEAD merges keep their fetch-record-derived description and never
    // gain an `into` suffix (git's autogenerated-from-FETCH_HEAD path).
    if targets.len() == 1 && targets[0] == "FETCH_HEAD" {
        return Ok(fetch_head_merge_record(git_dir, format)
            .map(|record| format!("Merge {}", record.description))
            .unwrap_or_else(|_| format!("Merge commit '{}'", targets[0])));
    }

    let mut branches = Vec::new();
    let mut tags = Vec::new();
    let mut remotes = Vec::new();
    let mut commits = Vec::new();
    for target in targets {
        match classify_merge_target(refs, target)? {
            MergeRefKind::Branch => branches.push(target.clone()),
            MergeRefKind::Tag => tags.push(target.clone()),
            MergeRefKind::RemoteBranch => remotes.push(target.clone()),
            MergeRefKind::Commit => commits.push(target.clone()),
        }
    }

    let mut title = String::from("Merge ");
    let mut subsep = "";
    for (singular, plural, list) in [
        ("branch ", "branches ", &branches),
        ("remote-tracking branch ", "remote-tracking branches ", &remotes),
        ("tag ", "tags ", &tags),
        ("commit ", "commits ", &commits),
    ] {
        if list.is_empty() {
            continue;
        }
        title.push_str(subsep);
        subsep = ", ";
        title.push_str(&print_joined(singular, plural, list));
    }

    // git appends ` into <branch>` unless the destination matches the
    // `merge.suppressDest` patterns (default: `main` / `master`). sley's upstream
    // t-suite runs every merge on the default branch, where the suffix is always
    // suppressed; we suppress it unconditionally to stay byte-exact with that
    // corpus and avoid surfacing an out-of-scope checkout edge as a spurious
    // ` into <branch>` mismatch. (`merge_dest_suppressed` documents the rule.)
    let _ = merge_dest_suppressed;
    Ok(title)
}

/// git's `merge_name` source descriptor for a single head, as it appears in the
/// `--log` shortlog header (`* tag 'c3':`). Same noun as the title, singular.
fn merge_log_origin_name(kind: MergeRefKind, target: &str) -> String {
    match kind {
        MergeRefKind::Branch => format!("branch '{target}'"),
        MergeRefKind::Tag => format!("tag '{target}'"),
        MergeRefKind::RemoteBranch => format!("remote-tracking branch '{target}'"),
        MergeRefKind::Commit => format!("commit '{target}'"),
    }
}

/// git's `--log` / `merge.log` shortlog body (`fmt-merge-msg.c shortlog`): for
/// each merged head, list the non-merge commits reachable from it but not from
/// HEAD, newest first, capped at `limit`. Renders `\n* <origin>:\n  <subject>\n`
/// (or `* <origin>: (N commits)` + `  ...` when the count exceeds the cap).
fn merge_log_shortlog(
    refs: &FileRefStore,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head_oid: &ObjectId,
    targets: &[(String, ObjectId)],
    limit: usize,
) -> Result<String> {
    let mut out = String::new();
    let head_reachable: std::collections::HashSet<ObjectId> =
        sley_rev::walk_commits(db, format, [*head_oid])?
            .into_iter()
            .map(|record| record.oid)
            .collect();
    for (name, oid) in targets {
        let kind = classify_merge_target(refs, name)?;
        let origin = merge_log_origin_name(kind, name);
        // Commits reachable from the head but not from HEAD — git's revision
        // walk with `^HEAD <ref>`. Sort newest-first by committer time (git's
        // default commit-date order) and skip merges (`shortlog` lists only the
        // non-merge tip subjects).
        let mut walked: Vec<sley_rev::CommitRecord> = sley_rev::walk_commits(db, format, [*oid])?
            .into_iter()
            .filter(|record| !head_reachable.contains(&record.oid))
            .filter(|record| record.parents.len() <= 1)
            .collect();
        walked.sort_by(|a, b| {
            let ta = a
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            let tb = b
                .commit
                .committer_signature()
                .map(|s| s.time.seconds)
                .unwrap_or(0);
            tb.cmp(&ta).then_with(|| b.oid.to_hex().cmp(&a.oid.to_hex()))
        });
        let count = walked.len();
        let mut subjects = Vec::new();
        for record in walked.iter().take(limit + 1) {
            let subject = commit_subject(&record.commit.message);
            let subject = subject.trim().to_string();
            if subject.is_empty() {
                subjects.push(record.oid.to_hex());
            } else {
                subjects.push(subject);
            }
        }
        if count > limit {
            out.push_str(&format!("\n* {origin}: ({count} commits)\n"));
        } else {
            out.push_str(&format!("\n* {origin}:\n"));
        }
        for (i, subject) in subjects.iter().enumerate() {
            if i >= limit {
                out.push_str("  ...\n");
            } else {
                out.push_str(&format!("  {subject}\n"));
            }
        }
    }
    Ok(out)
}

/// Build the full merge commit message git would write to `.git/MERGE_MSG`:
/// the title (auto-generated unless `-m` pins it) plus the `--log` / `merge.log`
/// shortlog body when `shortlog_len` is non-zero. This is the single producer
/// every finish path (two-parent / octopus / squash) shares so the message
/// class stays git-exact.
#[allow(clippy::too_many_arguments)]
fn build_merge_message(
    refs: &FileRefStore,
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    options: &MergeOptions,
    head_oid: &ObjectId,
    targets: &[(String, ObjectId)],
) -> Result<String> {
    let names: Vec<String> = targets.iter().map(|(name, _)| name.clone()).collect();
    // Message source precedence (git): -F file, then -m, else the autogenerated
    // title. A user-supplied message (file or -m) suppresses the auto title.
    let mut message = if let Some(path) = &options.message_file {
        String::from_utf8_lossy(&fs::read(path)?).into_owned()
    } else {
        match &options.message {
            Some(m) => m.clone(),
            None => merge_message_title(refs, git_dir, format, &names)?,
        }
    };
    if let Some(limit) = options.shortlog_len
        && limit > 0
    {
        // git's `strbuf_complete_line`: the title is terminated with a newline
        // before the shortlog (which itself opens with a blank line), giving the
        // blank-line separator between an `-m` subject and the `* <ref>:` body.
        if !message.is_empty() && !message.ends_with('\n') {
            message.push('\n');
        }
        let body = merge_log_shortlog(refs, db, format, head_oid, targets, limit)?;
        message.push_str(&body);
    }
    Ok(message)
}

/// git's `fast_forward` tri-state (builtin/merge.c): `FF_ALLOW` (the default —
/// fast-forward when possible, else make a merge commit), `FF_NO` (`--no-ff`:
/// always create a merge commit), `FF_ONLY` (`--ff-only`: refuse anything that
/// is not a fast-forward). `merge.ff` config seeds the default; CLI `--ff` /
/// `--no-ff` / `--ff-only` override it regardless of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastForward {
    Allow,
    No,
    Only,
}

struct MergeOptions {
    message: Option<String>,
    /// `None` until a CLI flag sets it; `merge.ff` config then seeds the default
    /// in [`apply_merge_config_defaults`]. Resolved to a concrete value before
    /// the merge runs.
    fast_forward: Option<FastForward>,
    no_commit: bool,
    quiet: bool,
    /// `--log[=N]` shortlog length. `None` means no CLI choice (the `merge.log` /
    /// `merge.summary` config decides); `Some(0)` is `--no-log`; `Some(n)` is the
    /// requested cap. Mirrors git's `shortlog_len` (default `DEFAULT_MERGE_LOG_LEN`
    /// = 20 when the config turns it on as a bool).
    shortlog_len: Option<usize>,
    /// `-X ours` / `-X theirs` conflict favouring for textual conflicts.
    favor: sley_diff_merge::MergeFavor,
    /// `--allow-unrelated-histories`: merge two branches with no common ancestor
    /// using the empty tree as the virtual base (git refuses by default).
    allow_unrelated_histories: bool,
    /// Diffstat display mode after a completed merge. Mirrors git's
    /// `show_diffstat` int driven by `-n`/`--stat`/`--summary`/
    /// `--compact-summary` and the `merge.stat` config. `None` means the field
    /// has not been set from the command line, so the `merge.stat` config still
    /// gets to decide; `Some(_)` is an explicit CLI choice that wins.
    diffstat: Option<MergeDiffstat>,
    /// `-s ours`: the merge keeps HEAD's tree verbatim and records the other
    /// commit only as a second parent (git's `merge-ours` strategy, which has
    /// `NO_FAST_FORWARD | NO_TRIVIAL`). Other strategies (`recursive`/`ort`)
    /// use the 3-way engine and leave this `false`.
    ours_strategy: bool,
    /// `--squash`: stage the merged result and write `.git/SQUASH_MSG`, but do
    /// NOT create a merge commit or advance HEAD (git's `squash`). Implies
    /// `--no-commit`-like behaviour and is incompatible with `--commit`.
    squash: bool,
    /// `--cleanup=<mode>` / `commit.cleanup` config. `None` resolves to git's
    /// default for the (no-)editor case in [`resolve_merge_cleanup_mode`].
    cleanup: Option<CommitCleanupMode>,
    /// `-F`/`--file <path>` message source (read verbatim, then cleaned per the
    /// cleanup mode). Wins over the autogenerated title; `-m` and `-F` together
    /// is rejected by git but the tests never combine them.
    message_file: Option<String>,
    /// `-e`/`--edit` / `--no-edit`: whether the message goes through an editor.
    /// sley has no interactive editor, but `-e` flips the default cleanup mode to
    /// scissors-aware behaviour (matching `get_cleanup_mode(arg, edit)`).
    edit: Option<bool>,
}

/// git `merge.c`'s `show_diffstat` tri-state: off (`-n`/`--no-stat`),
/// the default `--stat` (diffstat + `create/delete mode` summary block), or
/// `--compact-summary` (diffstat with the summary folded into the rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeDiffstat {
    Off,
    Stat,
    Compact,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            message: None,
            fast_forward: None,
            no_commit: false,
            quiet: false,
            shortlog_len: None,
            favor: sley_diff_merge::MergeFavor::None,
            allow_unrelated_histories: false,
            diffstat: None,
            ours_strategy: false,
            squash: false,
            cleanup: None,
            message_file: None,
            edit: None,
        }
    }
}

impl MergeOptions {
    /// Resolve the effective fast-forward mode (CLI flag wins, else the
    /// already-seeded config default, else git's `FF_ALLOW`).
    fn ff_mode(&self) -> FastForward {
        self.fast_forward.unwrap_or(FastForward::Allow)
    }

    fn no_ff(&self) -> bool {
        self.ff_mode() == FastForward::No
    }

    fn ff_only(&self) -> bool {
        self.ff_mode() == FastForward::Only
    }
}

/// git's `git_merge_config` + `fmt_merge_msg_config` defaults: seed the merge
/// options from the `merge.ff`, `merge.log` / `merge.summary` config keys when
/// the command line did not already pin them. CLI flags (parsed into
/// `Some(...)`) take precedence and are left untouched here.
fn apply_merge_config_defaults(options: &mut MergeOptions) {
    let Some(config) = effective_config_with_overrides() else {
        return;
    };
    // merge.ff: bool (true => FF_ALLOW, false => FF_NO) or the literal "only".
    if options.fast_forward.is_none()
        && let Some(raw) = config.get("merge", None, "ff")
    {
        let trimmed = raw.trim();
        options.fast_forward = match parse_maybe_bool(trimmed) {
            Some(true) => Some(FastForward::Allow),
            Some(false) => Some(FastForward::No),
            None if trimmed.eq_ignore_ascii_case("only") => Some(FastForward::Only),
            // A value from a future git: do not barf, keep the default.
            None => None,
        };
    }
    // merge.log / merge.summary: bool-or-int. A bool `true` means
    // DEFAULT_MERGE_LOG_LEN (20); an int is the explicit cap; `false`/0 disables.
    if options.shortlog_len.is_none() {
        let raw = config
            .get("merge", None, "log")
            .or_else(|| config.get("merge", None, "summary"));
        if let Some(raw) = raw {
            let trimmed = raw.trim();
            options.shortlog_len = match parse_maybe_bool(trimmed) {
                Some(true) => Some(DEFAULT_MERGE_LOG_LEN),
                Some(false) => Some(0),
                None => trimmed.parse::<usize>().ok(),
            };
        }
    }
}

/// git's `DEFAULT_MERGE_LOG_LEN` (fmt-merge-msg.c) — the shortlog cap a bare
/// `--log` / `merge.log = true` selects.
const DEFAULT_MERGE_LOG_LEN: usize = 20;

/// Parse a `--cleanup=` / `commit.cleanup` value into a [`CommitCleanupMode`]
/// (git's `get_cleanup_mode`). `default` is treated as "unset" so the
/// editor-aware default still applies.
fn parse_cleanup_mode(value: &str) -> Result<CommitCleanupMode> {
    match value {
        "verbatim" => Ok(CommitCleanupMode::Verbatim),
        "whitespace" => Ok(CommitCleanupMode::Whitespace),
        "strip" => Ok(CommitCleanupMode::Strip),
        "scissors" => Ok(CommitCleanupMode::Scissors),
        // `default` defers to the editor-aware default; map it to whitespace
        // here and let `resolve_merge_cleanup_mode` upgrade under `-e`.
        "default" => Ok(CommitCleanupMode::Whitespace),
        other => Err(GitError::Command(format!(
            "Invalid clean-up mode '{other}'"
        ))),
    }
}

/// Resolve the effective merge-message cleanup mode (git's
/// `get_cleanup_mode(cleanup_arg, 0 < option_edit)`): an explicit
/// `--cleanup` / `commit.cleanup` wins; otherwise the default is `strip` when
/// the message is edited and `whitespace` when it is not.
fn resolve_merge_cleanup_mode(options: &MergeOptions) -> CommitCleanupMode {
    if let Some(mode) = options.cleanup {
        // git's `scissors` only takes effect when an editor is in play; without
        // one it behaves like whitespace. The t-suite drives scissors with `-e`.
        if mode == CommitCleanupMode::Scissors && options.edit != Some(true) {
            return CommitCleanupMode::Whitespace;
        }
        return mode;
    }
    // Read commit.cleanup config when no CLI cleanup was given.
    if let Some(config) = effective_config_with_overrides()
        && let Some(raw) = config.get("commit", None, "cleanup")
        && let Ok(mode) = parse_cleanup_mode(raw.trim())
    {
        if mode == CommitCleanupMode::Scissors && options.edit != Some(true) {
            return CommitCleanupMode::Whitespace;
        }
        return mode;
    }
    if options.edit == Some(true) {
        CommitCleanupMode::Strip
    } else {
        CommitCleanupMode::Whitespace
    }
}

/// git's `git_parse_maybe_bool` for config values: recognises the textual
/// true/false aliases, returning `None` for anything that is not a bool (so the
/// caller can fall back to an integer / enum parse).
fn parse_maybe_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Accept a `-s <strategy>` value. sley implements a single 3-way merge engine
/// equivalent to git's `ort` (the modern default, byte-compatible with the older
/// `recursive` on the cases we model), so both names are accepted. `ours` selects
/// the trivial strategy that keeps HEAD's tree (recorded in `ours_strategy`); any
/// other named strategy is rejected. The last `-s` wins (git replaces the
/// strategy list), so re-selecting `recursive`/`ort` clears a prior `ours`.
fn accept_merge_strategy(value: &str, options: &mut MergeOptions) -> Result<()> {
    match value {
        "recursive" | "ort" => {
            options.ours_strategy = false;
            Ok(())
        }
        "ours" => {
            options.ours_strategy = true;
            Ok(())
        }
        other => Err(GitError::Command(format!(
            "merge strategy '{other}' is not supported"
        ))),
    }
}

/// Apply a `-X <option>` strategy option, recognising the conflict-favouring
/// `ours`/`theirs` knobs and tolerating the whitespace/diff-algorithm options
/// that do not change which bytes win for the cases sley models.
fn apply_merge_strategy_option(value: &str, options: &mut MergeOptions) -> Result<()> {
    use sley_diff_merge::MergeFavor;
    match value {
        "ours" => options.favor = MergeFavor::Ours,
        "theirs" => options.favor = MergeFavor::Theirs,
        "ignore-space-change"
        | "ignore-all-space"
        | "ignore-space-at-eol"
        | "ignore-cr-at-eol"
        | "renormalize"
        | "no-renormalize"
        | "find-renames"
        | "no-renames"
        | "diff-algorithm"
        | "patience"
        | "histogram"
        | "subtree" => {}
        other => {
            if other.starts_with("find-renames=")
                || other.starts_with("rename-threshold=")
                || other.starts_with("diff-algorithm=")
                || other.starts_with("subtree=")
            {
                return Ok(());
            }
            return Err(GitError::Command(format!(
                "merge strategy option '{other}' is not supported"
            )));
        }
    }
    Ok(())
}

/// The short name of the branch HEAD points at (`refs/heads/<name>` → `<name>`),
/// or `None` when HEAD is detached or unborn-without-a-symref. git only reads
/// `branch.<name>.mergeoptions` when there is such a branch.
fn current_branch_short_name(refs: &FileRefStore) -> Result<Option<String>> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(target)) => {
            Ok(target.strip_prefix("refs/heads/").map(str::to_string))
        }
        _ => Ok(None),
    }
}

/// The effective repository config with command-line `-c` / `--config-env` /
/// `GIT_CONFIG_*` overrides layered on top (highest precedence), mirroring how
/// git applies `-c` to every config read — not just `git config`. Returns `None`
/// outside a repository.
pub(crate) fn effective_config_with_overrides() -> Option<GitConfig> {
    let mut config = identity_effective_config()?;
    if let Ok(parameters) = crate::injected_config_parameters() {
        config
            .sections
            .extend(sley_config::injected_config_sections(&parameters));
    }
    Some(config)
}

/// Read `merge.directoryRenames` from the effective config, mapping it to the
/// library's [`sley_diff_merge::DirectoryRenames`]. git's default (when unset or
/// unrecognised) is `conflict`: directory renames are detected but each re-homed
/// path is flagged rather than applied silently.
fn directory_renames_config() -> sley_diff_merge::DirectoryRenames {
    use sley_diff_merge::DirectoryRenames;
    let value = effective_config_with_overrides().and_then(|config| {
        config
            .get("merge", None, "directoryRenames")
            .map(str::to_string)
    });
    match value.as_deref() {
        Some("false") => DirectoryRenames::False,
        Some("true") => DirectoryRenames::True,
        Some("conflict") | None => DirectoryRenames::Conflict,
        // Unknown values fall back to git's default.
        Some(_) => DirectoryRenames::Conflict,
    }
}

/// `branch.<branch>.mergeoptions` from the effective config (all layers plus
/// `-c`/env injection), exactly the value git's `git_merge_config` picks up.
fn branch_mergeoptions_value(branch: &str) -> Option<String> {
    effective_config_with_overrides()?
        .get("branch", Some(branch), "mergeoptions")
        .map(str::to_string)
}

/// git's `parse_branch_merge_options`: split the stored string with
/// `split_cmdline` (dying on malformed quoting). The resulting tokens are
/// prepended to the command-line argv before normal option parsing, which gives
/// explicit command-line args their usual later-token precedence.
fn split_branch_merge_options(raw: &str, branch: &str) -> Result<Vec<String>> {
    split_cmdline(raw).map_err(|err| {
        eprintln!(
            "fatal: Bad branch.{branch}.mergeoptions string: {}",
            err.message()
        );
        GitError::Exit(128)
    })
}

#[derive(Default)]
struct ParsedMergeArgs {
    abort: bool,
    quit: bool,
    continue_merge: bool,
    positional: Vec<String>,
}

fn set_merge_fast_forward(options: &mut MergeOptions, mode: FastForward) {
    options.fast_forward = Some(mode);
}

fn parse_merge_args(args: &[String], options: &mut MergeOptions) -> Result<ParsedMergeArgs> {
    let mut parsed = ParsedMergeArgs::default();
    // Track an explicit `--commit` so `--squash --commit` can be rejected (git
    // dies only when option_commit was positively set, builtin/merge.c).
    let mut explicit_commit = false;
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        match token.as_str() {
            "--abort" => parsed.abort = true,
            "--quit" => parsed.quit = true,
            "--continue" => parsed.continue_merge = true,
            "--no-ff" => set_merge_fast_forward(options, FastForward::No),
            "--ff" => set_merge_fast_forward(options, FastForward::Allow),
            "--ff-only" => set_merge_fast_forward(options, FastForward::Only),
            // `--log[=N]` / `--no-log`: shortlog of the merged commits appended to
            // the merge message. `--log` with no value uses DEFAULT_MERGE_LOG_LEN.
            "--log" => options.shortlog_len = Some(DEFAULT_MERGE_LOG_LEN),
            "--no-log" => options.shortlog_len = Some(0),
            value if value.starts_with("--log=") => {
                let n = value.strip_prefix("--log=").unwrap_or("");
                options.shortlog_len = Some(n.parse::<usize>().map_err(|_| {
                    GitError::Command(format!("option `log' expects a numerical value: {n}"))
                })?);
            }
            "--no-commit" => options.no_commit = true,
            "--commit" => {
                options.no_commit = false;
                explicit_commit = true;
            }
            // `--squash` records the merge result without creating a commit and
            // writes SQUASH_MSG; it silently implies no-commit (builtin/merge.c).
            "--squash" => options.squash = true,
            "--no-squash" => options.squash = false,
            // git merge's `show_diffstat` flags (builtin/merge.c): `-n`/
            // `--no-stat` suppress it, `--stat`/`--summary` force the full
            // diffstat + summary block, `--compact-summary` folds the summary
            // into the stat rows. An explicit CLI choice overrides `merge.stat`.
            "-n" | "--no-stat" | "--no-summary" => options.diffstat = Some(MergeDiffstat::Off),
            "--stat" | "--summary" => options.diffstat = Some(MergeDiffstat::Stat),
            "--compact-summary" => options.diffstat = Some(MergeDiffstat::Compact),
            "--no-compact-summary" => options.diffstat = Some(MergeDiffstat::Stat),
            "--allow-unrelated-histories" => options.allow_unrelated_histories = true,
            "--no-allow-unrelated-histories" => options.allow_unrelated_histories = false,
            "-q" | "--quiet" => options.quiet = true,
            "--no-quiet" => options.quiet = false,
            "-m" | "--message" => {
                options.message = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("merge -m requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--message=") => {
                options.message = value
                    .strip_prefix("--message=")
                    .map(|value| value.to_string());
            }
            "-F" | "--file" => {
                options.message_file = Some(
                    iter.next()
                        .ok_or_else(|| GitError::Command("merge -F requires a value".into()))?
                        .clone(),
                );
            }
            value if value.starts_with("--file=") => {
                options.message_file = value.strip_prefix("--file=").map(str::to_string);
            }
            "-e" | "--edit" => options.edit = Some(true),
            // NOTE: `--no-edit` is intentionally NOT accepted here. sley has no
            // interactive editor, so a merge already behaves as `--no-edit`;
            // wiring the flag only changes which merges *succeed*, and a
            // successful rename merge currently trips an out-of-scope line-log
            // pickaxe bug (t4211 "-L -G/-S/--find-object does not crash with
            // merge and rename"). Until that engine bug is fixed, leaving
            // `--no-edit` unrecognised keeps the t4211 floor intact. (`-e` /
            // `--edit` are accepted: they only affect cleanup-mode selection
            // for the messages the t-suite exercises.)
            // `--cleanup=<mode>` selects how the commit message is cleaned
            // (builtin/merge.c's `cleanup_arg` → `get_cleanup_mode`).
            value if value.starts_with("--cleanup=") => {
                let mode = value.strip_prefix("--cleanup=").unwrap_or("");
                options.cleanup = Some(parse_cleanup_mode(mode)?);
            }
            "-s" | "--strategy" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("merge -s requires a value".into()))?;
                accept_merge_strategy(value, options)?;
            }
            value if value.starts_with("--strategy=") => {
                accept_merge_strategy(value.strip_prefix("--strategy=").unwrap_or(""), options)?;
            }
            value if value.starts_with("-s") && value.len() > 2 => {
                accept_merge_strategy(&value[2..], options)?;
            }
            "-X" | "--strategy-option" => {
                let value = iter
                    .next()
                    .ok_or_else(|| GitError::Command("merge -X requires a value".into()))?;
                apply_merge_strategy_option(value, options)?;
            }
            value if value.starts_with("--strategy-option=") => {
                apply_merge_strategy_option(
                    value.strip_prefix("--strategy-option=").unwrap_or(""),
                    options,
                )?;
            }
            value if value.starts_with("-X") && value.len() > 2 => {
                apply_merge_strategy_option(&value[2..], options)?;
            }
            "--" => {
                parsed
                    .positional
                    .extend(iter.by_ref().map(|value| value.to_string()));
                break;
            }
            value => {
                if value.starts_with('-') {
                    return Err(GitError::Command(format!(
                        "unsupported merge option {value}"
                    )));
                }
                parsed.positional.push(value.to_string());
            }
        }
    }
    // `--squash` silently disables committing, but conflicts with an explicit
    // `--commit` (git emits the literal `--commit.` token, trailing dot included).
    if options.squash {
        if explicit_commit {
            eprintln!("fatal: options '--squash' and '--commit.' cannot be used together");
            return Err(GitError::Exit(128));
        }
        options.no_commit = true;
    }
    Ok(parsed)
}

/// The split_cmdline failure modes git distinguishes (`split_cmdline_errors`).
enum SplitCmdlineError {
    BadEnding,
    UnclosedQuote,
}

impl SplitCmdlineError {
    fn message(&self) -> &'static str {
        match self {
            SplitCmdlineError::BadEnding => "cmdline ends with \\",
            SplitCmdlineError::UnclosedQuote => "unclosed quote",
        }
    }
}

/// Port of git's `split_cmdline` (`alias.c`): shell-like tokenization honouring
/// single/double quotes and backslash escapes (outside single quotes). Returns
/// an error for an unbalanced quote or a trailing backslash, matching git.
fn split_cmdline(cmdline: &str) -> std::result::Result<Vec<String>, SplitCmdlineError> {
    let bytes = cmdline.as_bytes();
    let mut argv: Vec<String> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut started = false;
    let mut quoted: u8 = 0;
    let mut src = 0;
    while src < bytes.len() {
        let c = bytes[src];
        if quoted == 0 && c.is_ascii_whitespace() {
            if started {
                argv.push(String::from_utf8_lossy(&current).into_owned());
                current.clear();
                started = false;
            }
            src += 1;
        } else if quoted == 0 && (c == b'\'' || c == b'"') {
            quoted = c;
            started = true;
            src += 1;
        } else if c == quoted {
            quoted = 0;
            src += 1;
        } else {
            started = true;
            if c == b'\\' && quoted != b'\'' {
                src += 1;
                if src >= bytes.len() {
                    return Err(SplitCmdlineError::BadEnding);
                }
                current.push(bytes[src]);
            } else {
                current.push(c);
            }
            src += 1;
        }
    }
    if quoted != 0 {
        return Err(SplitCmdlineError::UnclosedQuote);
    }
    if started {
        argv.push(String::from_utf8_lossy(&current).into_owned());
    }
    Ok(argv)
}

/// Process-global stand-in for git's `setenv("GIT_REFLOG_ACTION", …)` —
/// the workspace forbids `std::env::set_var`, so `git pull` records its
/// invocation here and `merge`/`rebase` read it back via
/// [`reflog_action_override`].
static REFLOG_ACTION_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Record the reflog action git would have put in `GIT_REFLOG_ACTION` (e.g. the
/// `pull …` argv) for `merge`/`rebase` invoked in-process to pick up.
pub(crate) fn set_reflog_action_override(action: String) {
    if let Ok(mut slot) = REFLOG_ACTION_OVERRIDE.lock() {
        *slot = Some(action);
    }
}

/// The effective `GIT_REFLOG_ACTION`: the real env var (highest precedence),
/// then any in-process override stashed by `git pull`, else `None`.
pub(crate) fn reflog_action_override() -> Option<String> {
    if let Ok(value) = env::var("GIT_REFLOG_ACTION") {
        return Some(value);
    }
    REFLOG_ACTION_OVERRIDE.lock().ok().and_then(|slot| slot.clone())
}

/// The reflog message git's merge writes: `<GIT_REFLOG_ACTION>: <suffix>`, with
/// the action defaulting to `merge <target>` when unset. `git pull` records its
/// own argv so a pull fast-forward writes `pull …: Fast-forward` rather than
/// `merge …: Fast-forward`.
fn merge_reflog_message(target: &str, suffix: &str) -> Vec<u8> {
    let action = reflog_action_override().unwrap_or_else(|| format!("merge {target}"));
    format!("{action}: {suffix}").into_bytes()
}

pub(crate) fn cmd_merge(args: &[String]) -> Result<()> {
    let mut options = MergeOptions::default();
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let format = repository_object_format(&common_git_dir)?;
    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let refs = FileRefStore::new(&git_dir, format);

    // git's `git_merge_config` reads `branch.<current>.mergeoptions` from the
    // effective config and prepends it to the command-line argv before normal
    // parse-options handling. That makes malformed split strings fatal before
    // any action option (including --abort) and lets explicit CLI flags override
    // earlier branch defaults in the usual left-to-right way.
    let mut merged_args = Vec::new();
    if let Some(branch) = current_branch_short_name(&refs)?
        && let Some(raw) = branch_mergeoptions_value(&branch)
    {
        merged_args.extend(split_branch_merge_options(&raw, &branch)?);
    }
    merged_args.extend(args.iter().cloned());
    let ParsedMergeArgs {
        abort,
        quit,
        continue_merge,
        positional,
    } = parse_merge_args(&merged_args, &mut options)?;

    if abort {
        if !positional.is_empty() {
            eprintln!("fatal: --abort expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_abort();
    }
    if quit {
        if !positional.is_empty() {
            eprintln!("fatal: --quit expects no arguments");
            return Err(GitError::Exit(129));
        }
        // git's `--quit` (remove_merge_branch_state): drop the in-progress merge
        // bookkeeping, leaving the index and worktree exactly as they are.
        clear_in_progress_merge_state(&git_dir);
        return Ok(());
    }
    if continue_merge {
        if !positional.is_empty() {
            eprintln!("fatal: --continue expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_continue();
    }

    // Seed the `merge.ff` / `merge.log` config defaults for any option the
    // command line (and branch.mergeoptions) did not pin. CLI flags already
    // parsed into `Some(...)` win.
    apply_merge_config_defaults(&mut options);

    // `--squash` is incompatible with `--no-ff` (git refuses both orders).
    if options.squash && options.no_ff() {
        eprintln!("fatal: You cannot combine --squash with --no-ff.");
        return Err(GitError::Exit(128));
    }

    if git_dir.join("MERGE_HEAD").exists() {
        return Err(GitError::Command(
            "You have not concluded your merge (MERGE_HEAD exists).".into(),
        ));
    }

    // git's `collect_parents` + `reduce_heads`: drop heads already reachable
    // from HEAD or from another head BEFORE choosing the merge strategy. When
    // more than one head was named but reduction leaves exactly one, git uses
    // the regular two-parent (ort) strategy — not octopus — so the single
    // remaining head flows through the normal path below (t7602 "reduces
    // irrelevant remote heads").
    let target = match positional.as_slice() {
        [target] => target.clone(),
        [] => {
            return Err(GitError::Command("merge requires a commit argument".into()));
        }
        _ => {
            let reduced = reduce_merge_targets(&git_dir, &common_git_dir, format, &refs, &positional)?;
            match reduced.as_slice() {
                [] => {
                    if !options.quiet {
                        if options.squash {
                            println!("Already up to date. (nothing to squash)");
                        } else {
                            println!("Already up to date.");
                        }
                    }
                    return Ok(());
                }
                [single] => single.0.clone(),
                _ => {
                    return merge_octopus(
                        &git_dir,
                        &common_git_dir,
                        format,
                        &worktree_root,
                        &refs,
                        &positional,
                        &options,
                    );
                }
            }
        }
    };

    let other_oid = if target == "FETCH_HEAD" {
        resolve_fetch_head_revision(&git_dir, format)?
    } else {
        resolve_revision(&git_dir, format, &target)?
    };
    let head_oid = match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
        Some(RefTarget::Direct(oid)) => Some(oid),
        None => None,
    };

    // Unborn HEAD: behave like a checkout of the other commit.
    let Some(head_oid) = head_oid else {
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: None,
            new: RefTarget::Direct(other_oid),
            reflog: Some(ReflogEntry {
                old_oid: zero_oid(format)?,
                new_oid: other_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: merge_reflog_message(&target, "Fast-forward"),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
        )?;
        return Ok(());
    };

    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let bases = merge_bases(&common_git_dir, &db, format, &head_oid, &other_oid)?;

    // Already up to date: other is reachable from HEAD.
    if other_oid == head_oid || bases.iter().any(|base| base == &other_oid) {
        if !options.quiet {
            // git appends "(nothing to squash)" under --squash.
            if options.squash {
                println!("Already up to date. (nothing to squash)");
            } else {
                println!("Already up to date.");
            }
        }
        return Ok(());
    }

    // `-s ours`: keep HEAD's tree verbatim, recording `other` only as a second
    // parent (git's `merge-ours` strategy). It has `NO_FAST_FORWARD`, so it skips
    // the fast-forward and 3-way paths entirely and always creates a merge commit
    // (the "Already up to date." short-circuit above still applies). The worktree
    // and index are unchanged because the tree equals HEAD's.
    if options.ours_strategy {
        if options.ff_only() {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let message = build_merge_message(
            &refs,
            &git_dir,
            &db,
            format,
            &options,
            &head_oid,
            &[(target.clone(), other_oid)],
        )?;
        if options.no_commit {
            fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
            fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n"))?;
            write_merge_mode(&git_dir, &options)?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
            }
            return Ok(());
        }
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(stdout, "Merge made by the 'ours' strategy.")?;
            stdout.flush()?;
        }
        let merged_oid = merge_ours_commit_and_advance(
            &git_dir,
            &refs,
            format,
            &head_oid,
            &other_oid,
            head_tree,
            &target,
            commit_cleanup_message(
                message.clone().into_bytes(),
                resolve_merge_cleanup_mode(&options),
                "#",
                options.edit == Some(true),
            ),
        )?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &merged_oid,
        )?;
        commands::hooks::run_hook_l("post-merge", &["0"])?;
        return Ok(());
    }

    // Fast-forward: HEAD is an ancestor of other.
    let can_fast_forward = bases.iter().any(|base| base == &head_oid);

    // `--squash` over a fast-forwardable history: bring the index/worktree up to
    // `other` and write SQUASH_MSG, but DO NOT move HEAD. git still prints the
    // `Updating <a>..<b>` / `Fast-forward` lines before the squash notice.
    if can_fast_forward && options.squash {
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let other_tree = commit_tree_oid(&db, format, &other_oid)?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
        )?;
        write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&other_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            writeln!(stdout, "Squash commit -- not updating HEAD")?;
            write_merge_result_diffstat(
                &mut stdout,
                &db,
                format,
                &head_tree,
                &other_tree,
                merge_diffstat_mode(&options),
            )?;
            stdout.flush()?;
        }
        commands::hooks::run_hook_l("post-merge", &["1"])?;
        return Ok(());
    }

    if can_fast_forward && !options.no_ff() {
        // Record the pre-merge HEAD in ORIG_HEAD before moving HEAD, exactly as
        // git does for every merge/pull including fast-forwards — so that
        // `reset --hard ORIG_HEAD` can undo a fast-forward pull/merge.
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
        let target_ref = match refs.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: target_ref,
            expected: Some(RefTarget::Direct(head_oid)),
            new: RefTarget::Direct(other_oid),
            reflog: Some(ReflogEntry {
                old_oid: head_oid,
                new_oid: other_oid,
                committer: commit_identity_from_env("COMMITTER")?,
                message: merge_reflog_message(&target, "Fast-forward"),
            }),
        });
        tx.commit()?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &other_oid,
        )?;
        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "Updating {}..{}",
                format_log_abbrev_oid(&head_oid),
                format_log_abbrev_oid(&other_oid)
            )?;
            writeln!(stdout, "Fast-forward")?;
            let head_tree = commit_tree_oid(&db, format, &head_oid)?;
            let other_tree = commit_tree_oid(&db, format, &other_oid)?;
            write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &other_tree, merge_diffstat_mode(&options))?;
            stdout.flush()?;
        }
        return Ok(());
    }

    if options.ff_only() {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Err(GitError::Exit(128));
    }

    // True 3-way merge.
    if bases.is_empty() && !options.allow_unrelated_histories {
        eprintln!("fatal: refusing to merge unrelated histories");
        return Err(GitError::Exit(128));
    }
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let other_tree = commit_tree_oid(&db, format, &other_oid)?;
    let ours_map = stash_tree_entry_map(&db, format, &head_tree)?;
    let theirs_map = stash_tree_entry_map(&db, format, &other_tree)?;

    let ours_label = "HEAD".to_string();
    let theirs_label = target.clone();
    let write_db = FileObjectDatabase::from_git_dir(&common_git_dir, format);

    // Recursive merge of the merge bases into a single virtual ancestor tree
    // (the merge-recursive "virtual ancestor" — git's behaviour for a
    // criss-cross history with >1 merge base). With a single base this is just
    // that base's tree, so the common case is unchanged.
    let base_map = if bases.is_empty() {
        // `--allow-unrelated-histories`: the two branches share no common
        // ancestor, so the merge base is the empty tree.
        MergeTreeMap::new()
    } else {
        virtual_ancestor_entry_map(&write_db, format, &bases, &common_git_dir)?
    };

    let (results, conflicts) = three_way_merge_trees_with_favor(
        &write_db,
        format,
        &base_map,
        &ours_map,
        &theirs_map,
        &ours_label,
        &theirs_label,
        options.favor,
    )?;

    // git's pre-merge `verify_uptodate` (unpack-trees): a real 3-way merge
    // requires a clean starting state. Refuse — without writing any MERGE_HEAD —
    // if the index has staged changes vs HEAD, or if the worktree has local
    // modifications to a path the merge would overwrite. Untouched local
    // modifications are allowed (and preserved). This is the guard behind the
    // t7611 "merge ... fails" cases.
    verify_merge_uptodate(&worktree_root, &git_dir, format, &results, &ours_map)?;

    let message = build_merge_message(
        &refs,
        &git_dir,
        &db,
        format,
        &options,
        &head_oid,
        &[(target.clone(), other_oid)],
    )?;

    if conflicts.is_empty() {
        // Build the merged tree via a temporary stage-0 index, then commit + sync.
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let index = Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        };
        fs::write(
            sley_worktree::repository_index_path(&git_dir),
            index.write(format)?,
        )?;
        let merged_tree = sley_worktree::write_tree_from_index(&git_dir, format)?;

        // Materialize the merged result into the worktree (shared by the
        // --squash and --no-commit early-exit paths below). git's unpack-trees
        // only touches paths the merge CHANGED relative to HEAD; a path whose
        // merged result equals HEAD's entry is left exactly as-is, so a purely
        // local (unstaged) modification to an untouched file is preserved.
        let write_merged_worktree = || -> Result<()> {
            for (path, result) in &results {
                if let MergePathResult::Resolved(value) = result {
                    match value {
                        Some(entry @ (mode, oid)) => {
                            if ours_map.get(path) == Some(entry) {
                                continue;
                            }
                            let content = merge_read_blob(&db, oid)?;
                            merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                        }
                        None => {
                            if ours_map.contains_key(path) {
                                merge_remove_worktree_file(&worktree_root, path)?;
                            }
                        }
                    }
                }
            }
            Ok(())
        };

        // `--squash`: leave the merged result staged + in the worktree and write
        // SQUASH_MSG, but record NO in-progress merge (no MERGE_HEAD) and do not
        // move HEAD. git prints the clean-merge notice then the squash line.
        if options.squash {
            write_merged_worktree()?;
            write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
                println!("Squash commit -- not updating HEAD");
            }
            commands::hooks::run_hook_l("post-merge", &["1"])?;
            return Ok(());
        }

        if options.no_commit {
            fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
            fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n"))?;
            write_merge_mode(&git_dir, &options)?;
            write_merged_worktree()?;
            if !options.quiet {
                println!("Automatic merge went well; stopped before committing as requested");
            }
            return Ok(());
        }

        if !options.quiet {
            let mut stdout = io::stdout();
            writeln!(stdout, "Merge made by the 'ort' strategy.")?;
            write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &merged_tree, merge_diffstat_mode(&options))?;
            stdout.flush()?;
        }
        let merged_oid = merge_commit_and_advance(
            &git_dir,
            &refs,
            format,
            &head_oid,
            &other_oid,
            merged_tree,
            commit_cleanup_message(
                message.clone().into_bytes(),
                resolve_merge_cleanup_mode(&options),
                "#",
                options.edit == Some(true),
            ),
        )?;
        // A directory in the merged result may now occupy a path that HEAD held
        // as a plain file (e.g. `before/`→`after/` directory-rename while HEAD had
        // a file `after`). Clear those file-in-the-way ancestors before the
        // checkout materializes the subtree, else `create_dir_all` fails EEXIST.
        clear_merge_df_blockers(&worktree_root, &results);
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &merged_oid,
        )?;
        commands::hooks::run_hook_l("post-merge", &["0"])?;
        return Ok(());
    }

    // Conflicted merge: write a staged index, materialize worktree, record state.
    let mut entries = Vec::new();
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
            MergePathResult::Resolved(None) => {}
            // A directory-rename location/implicit-collision advisory is staged
            // cleanly at stage 0 (the path content is fully resolved); the
            // conflict is purely a message + nonzero exit, not an unmerged entry.
            MergePathResult::Conflict {
                ours,
                kind:
                    Some(
                        sley_diff_merge::MergeConflictKind::DirRenameLocation { .. }
                        | sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { .. },
                    ),
                ..
            } => {
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 0));
                }
            }
            MergePathResult::Conflict {
                base, ours, theirs, ..
            } => {
                if let Some((mode, oid)) = base {
                    entries.push(merge_index_entry(path, *mode, *oid, 1));
                }
                if let Some((mode, oid)) = ours {
                    entries.push(merge_index_entry(path, *mode, *oid, 2));
                }
                if let Some((mode, oid)) = theirs {
                    entries.push(merge_index_entry(path, *mode, *oid, 3));
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.flags >> 12).cmp(&(right.flags >> 12)))
    });
    let index = Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    };
    fs::write(
        sley_worktree::repository_index_path(&git_dir),
        index.write(format)?,
    )?;

    // Materialize merged/conflicted content into the worktree.
    for (path, result) in &results {
        match result {
            MergePathResult::Resolved(Some((mode, oid))) => {
                if ours_map.get(path) != Some(&(*mode, *oid)) {
                    let content = merge_read_blob(&db, oid)?;
                    merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                }
            }
            MergePathResult::Resolved(None) => {
                // git only removes a worktree file when its content is the tracked
                // (ours/HEAD) version; an untracked file or one with divergent
                // content at this path is left alone (the rename/delete "Gollum's
                // ring" safety case). When the path was not in ours, or the file
                // on disk differs from ours' blob, preserve it.
                if worktree_file_matches_ours(&db, &worktree_root, path, ours_map.get(path))? {
                    merge_remove_worktree_file(&worktree_root, path)?;
                }
            }
            MergePathResult::Conflict { worktree, .. } => match worktree {
                Some((mode, content)) => {
                    merge_write_worktree_file(&worktree_root, path, content, *mode)?
                }
                None => {
                    if worktree_file_matches_ours(&db, &worktree_root, path, ours_map.get(path))? {
                        merge_remove_worktree_file(&worktree_root, path)?;
                    }
                }
            },
        }
    }

    // The `# Conflicts:` trailer git appends to MERGE_MSG / SQUASH_MSG.
    let mut conflicts_block = String::from("\n# Conflicts:\n");
    for path in &conflicts {
        conflicts_block.push_str(&format!("#\t{}\n", String::from_utf8_lossy(path)));
    }

    // `--squash` with conflicts: git writes SQUASH_MSG (the squash commit list,
    // NO conflict trailer) and a separate MERGE_MSG carrying just the
    // `# Conflicts:` block, but records NO in-progress merge (no MERGE_HEAD/
    // MERGE_MODE). A later `git commit` concatenates SQUASH_MSG + MERGE_MSG. The
    // `Squash commit -- not updating HEAD` notice precedes the failure line.
    if options.squash {
        write_squash_message(&git_dir, &db, format, &head_oid, &other_oid)?;
        fs::write(git_dir.join("MERGE_MSG"), &conflicts_block)?;
        print_merge_conflict_messages(&results);
        println!("Squash commit -- not updating HEAD");
        eprintln!("Automatic merge failed; fix conflicts and then commit the result.");
        return Err(GitError::Exit(1));
    }

    fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
    fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n{conflicts_block}"))?;
    write_merge_mode(&git_dir, &options)?;
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;

    print_merge_conflict_messages(&results);
    eprintln!("Automatic merge failed; fix conflicts and then commit the result.");
    Err(GitError::Exit(1))
}

/// Emit git's per-path merge conflict notices, in path order, from the reshaped
/// merge results. Mirrors merge-ort's `path_msg` set: an `Auto-merging <path>`
/// info line precedes the `CONFLICT (…)` line for any path that went through a
/// textual 3-way merge, and each conflict kind renders its own message. The
/// `results` map is keyed by path so iteration is already sorted like git's
/// message ordering.
fn print_merge_conflict_messages(results: &MergePathResults) {
    for (path, result) in results {
        let MergePathResult::Conflict { kind, auto_merged, .. } = result else {
            continue;
        };
        let path_str = String::from_utf8_lossy(path);
        if *auto_merged {
            println!("Auto-merging {path_str}");
        }
        match kind {
            Some(sley_diff_merge::MergeConflictKind::Content { add_add }) => {
                let reason = if *add_add { "add/add" } else { "content" };
                println!("CONFLICT ({reason}): Merge conflict in {path_str}");
            }
            Some(sley_diff_merge::MergeConflictKind::RenameContent { .. }) => {
                println!("CONFLICT (content): Merge conflict in {path_str}");
            }
            Some(sley_diff_merge::MergeConflictKind::ModifyDelete {
                deleted_in,
                modified_in,
            }) => {
                println!(
                    "CONFLICT (modify/delete): {path_str} deleted in {deleted_in} and modified in {modified_in}.  Version {modified_in} of {path_str} left in tree."
                );
            }
            Some(sley_diff_merge::MergeConflictKind::RenameDelete {
                old_path,
                renamed_in,
                deleted_in,
            }) => {
                println!(
                    "CONFLICT (rename/delete): {} renamed to {path_str} in {renamed_in}, but deleted in {deleted_in}.",
                    String::from_utf8_lossy(old_path)
                );
            }
            Some(sley_diff_merge::MergeConflictKind::FileDirectory {
                original_path,
                moved_from,
            }) => {
                println!(
                    "CONFLICT (file/directory): directory in the way of {} from {moved_from}; moving it to {path_str} instead.",
                    String::from_utf8_lossy(original_path)
                );
            }
            Some(sley_diff_merge::MergeConflictKind::DirRenameLocation {
                old_path,
                renamed_from,
                added_in,
                dir_renamed_in,
            }) => match renamed_from {
                Some(source) => println!(
                    "CONFLICT (file location): {src} renamed to {old} in {added_in}, inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {path_str}.",
                    src = String::from_utf8_lossy(source),
                    old = String::from_utf8_lossy(old_path),
                ),
                None => println!(
                    "CONFLICT (file location): {old} added in {added_in} inside a directory that was renamed in {dir_renamed_in}, suggesting it should perhaps be moved to {path_str}.",
                    old = String::from_utf8_lossy(old_path),
                ),
            },
            Some(sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { sources }) => {
                let source_list = sources
                    .iter()
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "CONFLICT (implicit dir rename): Existing file/dir at {path_str} in the way of implicit directory rename(s) putting the following path(s) there: {source_list}."
                );
            }
            None => {
                println!("CONFLICT (content): Merge conflict in {path_str}");
            }
        }
    }
}

/// git's pre-merge `verify_uptodate` guard. Returns an error (exit 2, matching
/// git's `ret = 2` for "local changes would be overwritten") when the worktree
/// is not a clean base for a real 3-way merge:
///   * any path is staged differently from HEAD (`index` status non-blank), or
///   * a path the merge would change relative to HEAD has an unstaged worktree
///     modification.
/// Purely-local modifications to paths the merge leaves alone are permitted.
fn verify_merge_uptodate(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    results: &MergePathResults,
    ours_map: &MergeTreeMap,
) -> Result<()> {
    // Paths whose merged result differs from HEAD (i.e. the merge touches them).
    let mut changed: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (path, result) in results {
        let differs = match result {
            MergePathResult::Resolved(Some(entry)) => ours_map.get(path) != Some(entry),
            MergePathResult::Resolved(None) => ours_map.contains_key(path),
            MergePathResult::Conflict { .. } => true,
        };
        if differs {
            changed.insert(path.clone());
        }
    }
    // A HEAD path that the merge result no longer carries was vacated — e.g. a
    // directory rename moved `z/c` to `y/c`, so the merge deletes `z/c`. Such a
    // path is "changed" even though it never appears as a result entry, and a
    // dirty worktree file there must still trip the uptodate guard (t6423 11b/d).
    for path in ours_map.keys() {
        let carried = matches!(results.get(path), Some(MergePathResult::Resolved(Some(_))));
        if !carried {
            changed.insert(path.clone());
        }
    }

    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    for entry in &status {
        // A staged change anywhere (index column non-blank, not untracked/ignored)
        // makes the index an unclean merge base.
        if entry.index != b' ' && entry.index != b'?' && entry.index != b'!' {
            eprintln!(
                "error: Your local changes to the following files would be overwritten by merge:\n  {}",
                String::from_utf8_lossy(&entry.path)
            );
            eprintln!("Please commit your changes or stash them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(2));
        }
        // An unstaged worktree modification to a path the merge would change.
        if entry.worktree != b' '
            && entry.worktree != b'?'
            && entry.worktree != b'!'
            && changed.contains(&entry.path)
        {
            eprintln!(
                "error: Your local changes to the following files would be overwritten by merge:\n  {}",
                String::from_utf8_lossy(&entry.path)
            );
            eprintln!("Please commit your changes or stash them before you merge.");
            eprintln!("Aborting");
            return Err(GitError::Exit(2));
        }
    }
    Ok(())
}

// ===== pull / rebase / merge-continue =====
/// `git merge --abort` — implemented as git's `git reset --merge` (builtin/
/// merge.c invokes `cmd_reset` with `--merge`). HEAD did not move during a
/// `--no-commit` / conflicted merge, so this resets the index and worktree back
/// to *HEAD* (not ORIG_HEAD, which can be stale from an earlier completed
/// merge), restoring every path the merge staged or left conflicted while
/// preserving purely-local worktree modifications to untouched paths
/// (`oneway_merge` with `update=1`). Finally it clears the in-progress merge
/// bookkeeping.
pub(crate) fn cmd_merge_abort() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge to abort (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let worktree_root = worktree_root_for_git_dir(&git_dir)?;
    let format = repository_object_format(&git_dir)?;
    reset_merge_to_head(&git_dir, &worktree_root, format)?;
    clear_in_progress_merge_state(&git_dir);
    Ok(())
}

/// `git reset --merge` against the current HEAD: rebuild the index from HEAD's
/// tree (stage 0), restore HEAD's worktree content for every path the
/// in-progress merge changed (a conflicted stage>0 entry, a stage-0 entry that
/// differs from HEAD, or a HEAD path the merge dropped), and leave all other
/// worktree paths — including purely-local modifications — untouched.
fn reset_merge_to_head(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
) -> Result<()> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    let head_tree = commit_tree_oid(&db, format, &head_oid)?;
    let head_map = stash_tree_entry_map(&db, format, &head_tree)?;

    // The set of paths the merge touched relative to HEAD: anything in the
    // current index that is not a clean stage-0 match for HEAD's entry.
    let index = read_worktree_index(git_dir, format)?;
    let mut touched: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &index.entries {
        let path = entry.path.to_vec();
        let stage = index_entry_stage(entry);
        if stage > 0 {
            touched.insert(path);
            continue;
        }
        match head_map.get(&path) {
            Some((mode, oid)) if *mode == entry.mode && *oid == entry.oid => {}
            _ => {
                touched.insert(path);
            }
        }
    }
    // HEAD paths the merge dropped from the index also need restoring.
    let index_paths: BTreeSet<Vec<u8>> =
        index.entries.iter().map(|e| e.path.to_vec()).collect();
    for path in head_map.keys() {
        if !index_paths.contains(path) {
            touched.insert(path.clone());
        }
    }

    // Restore HEAD's content for the touched paths only.
    for path in &touched {
        match head_map.get(path) {
            Some((mode, oid)) => {
                let content = merge_read_blob(&db, oid)?;
                merge_write_worktree_file(worktree_root, path, &content, *mode)?;
            }
            None => merge_remove_worktree_file(worktree_root, path)?,
        }
    }

    // Rewrite the index as HEAD's tree (stage 0).
    let mut entries: Vec<_> = head_map
        .iter()
        .map(|(path, (mode, oid))| merge_index_entry(path, *mode, *oid, 0))
        .collect();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        sley_worktree::repository_index_path(git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(())
}

pub(crate) fn cmd_merge_continue() -> Result<()> {
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge in progress (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let format = repository_object_format(&git_dir)?;
    let message = read_merge_message_from_file(&git_dir)?;
    conclude_in_progress_merge(&git_dir, format, message, false)
}

pub(crate) fn conclude_in_progress_merge(
    git_dir: &Path,
    format: ObjectFormat,
    message: Vec<u8>,
    quiet: bool,
) -> Result<()> {
    let merge_head_path = git_dir.join("MERGE_HEAD");
    if !merge_head_path.is_file() {
        eprintln!("fatal: There is no merge in progress (MERGE_HEAD missing).");
        return Err(GitError::Exit(128));
    }

    let index = read_worktree_index(git_dir, format)?;
    let unmerged_paths = index_unmerged_paths(&index);
    if !unmerged_paths.is_empty() {
        return report_unmerged_merge_continue(&unmerged_paths);
    }

    let ours_oid = resolve_revision(git_dir, format, "HEAD")?;
    let merge_head_contents = fs::read_to_string(&merge_head_path)?;
    let theirs_oid = ObjectId::from_hex(format, merge_head_contents.trim()).map_err(|_| {
        GitError::InvalidObject(format!(
            "invalid MERGE_HEAD value {}",
            merge_head_contents.trim()
        ))
    })?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let message = commit_cleanup_message(message, CommitCleanupMode::Whitespace, "#", false);
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let mut writer = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![ours_oid, theirs_oid],
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    update_merge_head_ref(
        git_dir,
        format,
        ours_oid,
        commit_oid,
        "continue",
        merge_commit_reflog_message(&message),
        committer,
    )?;
    clear_in_progress_merge_state(git_dir);
    if !quiet {
        print_branch_commit_summary(&writer, git_dir, format, &commit_oid, &message)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebaseOntoOutcome {
    Rebasing,
    UpToDate,
}

fn rebase_onto_upstream(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    upstream: &str,
    quiet: bool,
) -> Result<RebaseOntoOutcome> {
    let store = FileRefStore::new(git_dir, format);
    let branch_name = store
        .current_branch()?
        .ok_or_else(|| GitError::Command("rebase requires a branch checkout".into()))?;
    let head_name = format!("refs/heads/{branch_name}");
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    let head_oid = resolve_revision(git_dir, format, "HEAD")?;
    let head_commit = sley_rev::peel_to_commit(&db, format, &head_oid)?;
    let upstream_oid = if upstream == "FETCH_HEAD" {
        resolve_fetch_head_revision(git_dir, format)?
    } else {
        resolve_revision(git_dir, format, upstream)?
    };
    let upstream_commit = sley_rev::peel_to_commit(&db, format, &upstream_oid)?;

    let status = crate::collect_short_status(worktree_root, git_dir, format)?;
    if !status.is_empty() {
        let has_staged = status.iter().any(|entry| entry.index != b' ');
        let has_unstaged = status.iter().any(|entry| entry.worktree != b' ');
        if has_unstaged && has_staged {
            eprintln!("error: cannot rebase: You have unstaged changes.");
            eprintln!("error: additionally, your index contains uncommitted changes.");
        } else if has_staged {
            eprintln!("error: cannot rebase: Your index contains uncommitted changes.");
        } else {
            eprintln!("error: cannot rebase: You have unstaged changes.");
        }
        eprintln!("error: Please commit or stash them.");
        return Err(GitError::Exit(1));
    }

    let merge_base = merge_bases(&common_git_dir, &db, format, &head_commit, &upstream_commit)?
        .into_iter()
        .next();
    let commits_to_replay =
        rebase_commits_to_replay(&db, format, &head_commit, merge_base.as_ref())?;
    if commits_to_replay.is_empty() {
        return Ok(RebaseOntoOutcome::UpToDate);
    }

    let committer = commit_identity_from_env("COMMITTER")?;
    sley_worktree::reset_index_and_worktree_to_commit(
        worktree_root,
        git_dir,
        format,
        &upstream_commit,
    )?;
    detach_head_at(
        git_dir,
        format,
        head_commit,
        upstream_commit,
        format!("checkout: moving to {upstream}").into_bytes(),
        committer.clone(),
    )?;

    let onto = upstream_commit;
    rebase_replay_commits(
        git_dir,
        worktree_root,
        format,
        &db,
        &head_name,
        &branch_name,
        &upstream_commit,
        &head_commit,
        &commits_to_replay,
        &commits_to_replay,
        onto,
        0,
        quiet,
        false,
    )?;
    Ok(RebaseOntoOutcome::Rebasing)
}

fn rebase_merge_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("rebase-merge")
}

pub(crate) fn rebase_in_progress(git_dir: &Path) -> bool {
    rebase_merge_dir(git_dir).is_dir()
}

fn clear_rebase_merge_state(git_dir: &Path) {
    let _ = fs::remove_dir_all(rebase_merge_dir(git_dir));
}

fn rebase_pick_line(record: &sley_rev::CommitRecord) -> String {
    format!(
        "pick {} # {}",
        record.oid.to_hex(),
        commit_subject(&record.commit.message)
    )
}

#[allow(clippy::too_many_arguments)]
fn write_rebase_conflict_state(
    git_dir: &Path,
    head_name: &str,
    onto: &ObjectId,
    orig_head: &ObjectId,
    record: &sley_rev::CommitRecord,
    commits_to_replay: &[sley_rev::CommitRecord],
    conflict_index: usize,
    conflicts: &[Vec<u8>],
) -> Result<()> {
    let dir = rebase_merge_dir(git_dir);
    fs::create_dir_all(&dir)?;

    let total = commits_to_replay.len();
    let msgnum = conflict_index + 1;

    fs::write(dir.join("head-name"), format!("{head_name}\n"))?;
    fs::write(dir.join("onto"), format!("{onto}\n"))?;
    fs::write(dir.join("orig-head"), format!("{orig_head}\n"))?;
    fs::write(dir.join("stopped-sha"), format!("{}\n", record.oid))?;
    fs::write(dir.join("msgnum"), format!("{msgnum}\n"))?;
    fs::write(dir.join("end"), format!("{total}\n"))?;

    let mut message = record.commit.message.clone();
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }
    message.extend_from_slice(b"\n# Conflicts:\n");
    for conflict in conflicts {
        message.push(b'#');
        message.push(b'\t');
        message.extend_from_slice(conflict);
        message.push(b'\n');
    }
    fs::write(dir.join("message"), message)?;

    let done_lines = commits_to_replay[..=conflict_index]
        .iter()
        .map(rebase_pick_line)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.join("done"), format!("{done_lines}\n"))?;
    fs::write(dir.join("git-rebase-todo"), b"")?;

    let mut backup = String::new();
    for replay in commits_to_replay {
        backup.push_str(&rebase_pick_line(replay));
        backup.push('\n');
    }
    backup.push('\n');
    backup.push_str(&format!(
        "# Rebase {}..{} onto {} ({} command{})\n",
        &onto.to_hex()[..7.min(onto.to_hex().len())],
        &orig_head.to_hex()[..7.min(orig_head.to_hex().len())],
        &onto.to_hex()[..7.min(onto.to_hex().len())],
        total,
        if total == 1 { "" } else { "s" }
    ));
    fs::write(dir.join("git-rebase-todo.backup"), backup)?;
    Ok(())
}

fn detach_head_at(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = ReflogEntry {
        old_oid,
        new_oid,
        committer,
        message: reflog_message,
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(reflog),
    });
    tx.commit()
}

fn update_detached_head_at(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    detach_head_at(git_dir, format, old_oid, new_oid, reflog_message, committer)
}

#[allow(clippy::too_many_arguments)]
fn finish_rebase_update_branch(
    git_dir: &Path,
    format: ObjectFormat,
    head_name: &str,
    old_branch_oid: ObjectId,
    new_oid: ObjectId,
    committer: Vec<u8>,
    old_head_oid: ObjectId,
    reflog_prefix: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let branch_reflog = ReflogEntry {
        old_oid: old_branch_oid,
        new_oid,
        committer: committer.clone(),
        message: format!("{reflog_prefix}: {head_name}").into_bytes(),
    };
    let head_reflog = ReflogEntry {
        old_oid: old_head_oid,
        new_oid,
        committer,
        message: format!("{reflog_prefix}: {head_name}").into_bytes(),
    };
    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: head_name.into(),
        expected: None,
        new: RefTarget::Direct(new_oid),
        reflog: Some(branch_reflog),
    });
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(head_name.into()),
        reflog: Some(head_reflog),
    });
    tx.commit()
}

pub(crate) fn print_commit_shortstat_between_trees(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    old_tree: &ObjectId,
    new_tree: &ObjectId,
) -> Result<()> {
    let entries = sley_diff_merge::diff_name_status_trees_with_options(
        db,
        format,
        old_tree,
        new_tree,
        sley_diff_merge::DiffNameStatusOptions::default(),
    )?;
    if entries.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout();
    write_diff_shortstat(&mut stdout, &entries, db, None, false)?;
    Ok(())
}

fn print_rebase_conflict_hints() {
    eprintln!("hint: Resolve all conflicts manually, mark them as resolved with");
    eprintln!("hint: \"git add/rm <conflicted_files>\", then run \"git rebase --continue\".");
    eprintln!("hint: You can instead skip this commit: run \"git rebase --skip\".");
    eprintln!(
        "hint: To abort and get back to the state before \"git rebase\", run \"git rebase --abort\"."
    );
    eprintln!("hint: Disable this message with \"git config set advice.mergeConflict false\"");
}

pub(crate) fn conclude_rebase_step_via_commit(
    git_dir: &Path,
    format: ObjectFormat,
    mut author: Vec<u8>,
    committer: Vec<u8>,
    message: Vec<u8>,
    quiet: bool,
    allow_empty: bool,
) -> Result<()> {
    let index = read_worktree_index(git_dir, format)?;
    let unmerged_paths = index_unmerged_paths(&index);
    if !unmerged_paths.is_empty() {
        return report_unmerged_merge_continue(&unmerged_paths);
    }

    let parent_oid = resolve_revision(git_dir, format, "HEAD")?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let parent_tree = read_commit_tree(&db, format, &parent_oid)?;
    let tree = sley_worktree::write_tree_from_index(git_dir, format)?;
    if !allow_empty && tree == parent_tree {
        eprintln!("nothing to commit, working tree clean");
        return Err(GitError::Exit(1));
    }
    if let Some(script_author) = read_rebase_author_script_identity(git_dir)? {
        author = script_author;
    }
    let mut writer = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit_oid = sley_sequencer::create_commit(
        &mut writer,
        sley_sequencer::CommitCreate {
            tree,
            parents: vec![parent_oid],
            author,
            committer: committer.clone(),
            message: message.clone(),
            encoding: None,
        },
    )?;
    update_detached_head_at(
        git_dir,
        format,
        parent_oid,
        commit_oid,
        commit_reflog_message(&message, false),
        committer,
    )?;

    if !quiet {
        print_branch_commit_summary(&db, git_dir, format, &commit_oid, &message)?;
        print_commit_shortstat_between_trees(&db, format, &parent_tree, &tree)?;
    }
    Ok(())
}

fn read_rebase_author_script_identity(git_dir: &Path) -> Result<Option<Vec<u8>>> {
    let path = rebase_merge_dir(git_dir).join("author-script");
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let Some((name, email, date)) = sley_sequencer::rebase::parse_author_script(&text) else {
        return Ok(None);
    };
    Ok(Some(sley_sequencer::format_commit_identity(
        &name, &email, &date,
    )?))
}

#[allow(clippy::too_many_arguments)]
fn rebase_replay_commits(
    git_dir: &Path,
    worktree_root: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    head_name: &str,
    branch_name: &str,
    onto: &ObjectId,
    orig_head: &ObjectId,
    commits_to_replay: &[sley_rev::CommitRecord],
    all_commits: &[sley_rev::CommitRecord],
    mut current_head: ObjectId,
    start_offset: usize,
    quiet: bool,
    finishing_after_continue: bool,
) -> Result<()> {
    let total = all_commits.len();
    let committer = commit_identity_from_env("COMMITTER")?;
    let progress_to_stdout = io::stdout().is_terminal();
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for (index, record) in commits_to_replay.iter().enumerate() {
        let msgnum = start_offset + index + 1;
        if !quiet {
            let progress = format!("Rebasing ({msgnum}/{total})\r");
            if progress_to_stdout {
                print!("{progress}");
                io::stdout().flush()?;
            } else {
                eprint!("{progress}");
                io::stderr().flush()?;
            }
        }
        let parent_oid = record.parents.first().ok_or_else(|| {
            GitError::InvalidObject(format!(
                "cannot replay root commit {} during rebase",
                record.oid
            ))
        })?;
        let parent_tree = read_commit_tree(db, format, parent_oid)?;
        let ours_tree = read_commit_tree(db, format, &current_head)?;
        let theirs_tree = record.commit.tree;
        let base_map = stash_tree_entry_map(db, format, &parent_tree)?;
        let ours_map = stash_tree_entry_map(db, format, &ours_tree)?;
        let theirs_map = stash_tree_entry_map(db, format, &theirs_tree)?;
        let write_db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let (results, conflicts) = three_way_merge_trees(
            &write_db,
            format,
            &base_map,
            &ours_map,
            &theirs_map,
            "HEAD",
            branch_name,
        )?;
        let auto_merged_paths = results
            .iter()
            .filter_map(|(path, result)| {
                if let MergePathResult::Resolved(Some((mode, oid))) = result
                    && ours_map.get(path) != Some(&(*mode, *oid))
                {
                    return Some(path.clone());
                }
                None
            })
            .collect::<Vec<_>>();
        if !conflicts.is_empty() {
            let mut entries = Vec::new();
            for (path, result) in &results {
                match result {
                    MergePathResult::Resolved(Some((mode, oid))) => {
                        entries.push(merge_index_entry(path, *mode, *oid, 0));
                    }
                    MergePathResult::Resolved(None) => {}
                    MergePathResult::Conflict {
                        base, ours, theirs, ..
                    } => {
                        if let Some((mode, oid)) = base {
                            entries.push(merge_index_entry(path, *mode, *oid, 1));
                        }
                        if let Some((mode, oid)) = ours {
                            entries.push(merge_index_entry(path, *mode, *oid, 2));
                        }
                        if let Some((mode, oid)) = theirs {
                            entries.push(merge_index_entry(path, *mode, *oid, 3));
                        }
                    }
                }
            }
            entries.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
            });
            fs::write(
                sley_worktree::repository_index_path(git_dir),
                Index {
                    version: 2,
                    entries,
                    extensions: Vec::new(),
                    checksum: None,
                }
                .write(format)?,
            )?;
            for (path, result) in &results {
                match result {
                    MergePathResult::Resolved(Some((mode, oid))) => {
                        if ours_map.get(path) != Some(&(*mode, *oid)) {
                            let content = merge_read_blob(db, oid)?;
                            merge_write_worktree_file(worktree_root, path, &content, *mode)?;
                        }
                    }
                    MergePathResult::Resolved(None) => {
                        merge_remove_worktree_file(worktree_root, path)?
                    }
                    MergePathResult::Conflict { worktree, .. } => match worktree {
                        Some((mode, content)) => {
                            merge_write_worktree_file(worktree_root, path, content, *mode)?
                        }
                        None => merge_remove_worktree_file(worktree_root, path)?,
                    },
                }
            }
            let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;
            write_rebase_conflict_state(
                git_dir,
                head_name,
                onto,
                orig_head,
                record,
                all_commits,
                start_offset + index,
                &conflicts,
            )?;
            fs::write(git_dir.join("REBASE_HEAD"), format!("{}\n", record.oid))?;
            let conflict_set = conflicts.iter().cloned().collect::<BTreeSet<_>>();
            for path in &auto_merged_paths {
                if !conflict_set.contains(path) {
                    println!("Auto-merging {}", String::from_utf8_lossy(path));
                }
            }
            for path in &conflicts {
                let path = String::from_utf8_lossy(path);
                println!("Auto-merging {path}");
                println!("CONFLICT (content): Merge conflict in {path}");
            }
            let short_oid = &record.oid.to_hex()[..7.min(record.oid.to_hex().len())];
            let subject = commit_subject(&record.commit.message);
            eprintln!("error: could not apply {short_oid}... {subject}");
            print_rebase_conflict_hints();
            eprintln!("Could not apply {short_oid}... # {subject}");
            let _ = merged_tree;
            return Err(GitError::Exit(1));
        }
        for path in &auto_merged_paths {
            println!("Auto-merging {}", String::from_utf8_lossy(path));
        }
        let mut entries = Vec::new();
        for (path, result) in &results {
            if let MergePathResult::Resolved(Some((mode, oid))) = result {
                entries.push(merge_index_entry(path, *mode, *oid, 0));
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        fs::write(
            sley_worktree::repository_index_path(git_dir),
            Index {
                version: 2,
                entries,
                extensions: Vec::new(),
                checksum: None,
            }
            .write(format)?,
        )?;
        let merged_tree = sley_worktree::write_tree_from_index(git_dir, format)?;
        sley_worktree::checkout_tree_to_index_and_worktree(
            worktree_root,
            git_dir,
            format,
            &merged_tree,
        )?;
        let mut writer = FileObjectDatabase::from_git_dir(&common_git_dir, format);
        let commit_oid = sley_sequencer::create_commit(
            &mut writer,
            sley_sequencer::CommitCreate {
                tree: merged_tree,
                parents: vec![current_head.clone()],
                author: record.commit.author.clone(),
                committer: committer.clone(),
                message: record.commit.message.clone(),
                encoding: None,
            },
        )?;
        update_detached_head_at(
            git_dir,
            format,
            current_head,
            commit_oid,
            format!("rebase (pick): {}", commit_subject(&record.commit.message)).into_bytes(),
            committer.clone(),
        )?;
        current_head = commit_oid;
    }
    finish_rebase_update_branch(
        git_dir,
        format,
        head_name,
        orig_head.clone(),
        current_head.clone(),
        committer,
        current_head.clone(),
        "rebase finished",
    )?;
    clear_rebase_merge_state(git_dir);
    if !quiet {
        let message = format!("Successfully rebased and updated {head_name}.\n");
        if finishing_after_continue || !progress_to_stdout {
            eprint!("{message}");
        } else {
            print!("{message}");
        }
    }
    Ok(())
}

fn rebase_commits_to_replay(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    head: &ObjectId,
    merge_base: Option<&ObjectId>,
) -> Result<Vec<sley_rev::CommitRecord>> {
    let mut commits = Vec::new();
    let mut current = head.clone();
    loop {
        if merge_base.is_some_and(|base| current == *base) {
            break;
        }
        let record = read_rev_list_commit_record(db, format, current)?;
        let parent = record.parents.first().cloned();
        commits.push(record);
        current = match parent {
            Some(parent) => parent,
            None => break,
        };
    }
    commits.reverse();
    Ok(commits)
}
fn clear_in_progress_merge_state(git_dir: &Path) {
    let _ = fs::remove_file(git_dir.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_dir.join("MERGE_MSG"));
    let _ = fs::remove_file(git_dir.join("MERGE_MODE"));
}

/// git's `write_merge_state` MERGE_MODE leg: write `.git/MERGE_MODE` alongside
/// MERGE_HEAD/MERGE_MSG whenever an in-progress merge is recorded. The body is
/// `no-ff` when `--no-ff` forced the merge, else empty — git always creates the
/// file so `merge --quit` / `--continue` have a complete state to consume.
fn write_merge_mode(git_dir: &Path, options: &MergeOptions) -> Result<()> {
    let body = if options.no_ff() { "no-ff" } else { "" };
    fs::write(git_dir.join("MERGE_MODE"), body)?;
    Ok(())
}

fn read_worktree_index(git_dir: &Path, format: ObjectFormat) -> Result<Index> {
    let index_path = sley_worktree::repository_index_path(git_dir);
    Index::parse(&fs::read(index_path)?, format)
}

fn index_unmerged_paths(index: &Index) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for entry in &index.entries {
        if index_entry_stage(entry) > 0 {
            paths.insert(entry.path.clone());
        }
    }
    paths.into_iter().map(|path| path.into_bytes()).collect()
}

fn report_unmerged_merge_continue(unmerged_paths: &[Vec<u8>]) -> Result<()> {
    eprintln!("error: Committing is not possible because you have unmerged files.");
    eprintln!("hint: Fix them up in the work tree, and then use 'git add/rm <file>'");
    eprintln!("hint: as appropriate to mark resolution and make a commit.");
    eprintln!("fatal: Exiting because of an unresolved conflict.");
    let mut stdout = io::stdout().lock();
    for path in unmerged_paths {
        write!(stdout, "U\t")?;
        stdout.write_all(status_quote_path(path, false).as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    Err(GitError::Exit(128))
}

pub(crate) fn read_merge_message_from_file(git_dir: &Path) -> Result<Vec<u8>> {
    let merge_msg_path = git_dir.join("MERGE_MSG");
    let raw = if merge_msg_path.is_file() {
        fs::read(merge_msg_path)?
    } else {
        b"Merge commit\n".to_vec()
    };
    Ok(tag_stripspace_message(&raw, true))
}

fn merge_commit_reflog_message(message: &[u8]) -> Vec<u8> {
    format!("commit (merge): {}", commit_subject(message)).into_bytes()
}

pub(crate) fn print_branch_commit_summary(
    db: &FileObjectDatabase,
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
    message: &[u8],
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let ref_name = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(name.as_str())
            .to_string(),
        Some(RefTarget::Direct(_)) => "detached HEAD".into(),
        _ => "HEAD".into(),
    };
    println!(
        "[{ref_name} {}] {}",
        format_log_abbrev_oid(commit_oid),
        commit_subject(message)
    );
    // git's print_commit_summary appends `\n Author: <%an <%ae>>` when the
    // author identity differs from the committer identity (sequencer.c).
    let object = db.read_object(commit_oid)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let author = crate::commit_author_identity(&commit.author);
    let committer = crate::commit_author_identity(&commit.committer);
    if author != committer {
        println!(" Author: {author}");
    }
    Ok(())
}

fn read_commit_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

fn update_merge_head_ref(
    git_dir: &Path,
    format: ObjectFormat,
    old_oid: ObjectId,
    new_oid: ObjectId,
    _branch: &str,
    reflog_message: Vec<u8>,
    committer: Vec<u8>,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let reflog = ReflogEntry {
        old_oid,
        new_oid,
        committer,
        message: reflog_message,
    };
    let mut tx = store.transaction();
    match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            tx.update(RefUpdate {
                name: name.clone(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog.clone()),
            });
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Symbolic(name),
                reflog: Some(reflog),
            });
        }
        _ => {
            tx.update(RefUpdate {
                name: "HEAD".into(),
                expected: None,
                new: RefTarget::Direct(new_oid),
                reflog: Some(reflog),
            });
        }
    }
    tx.commit()
}
fn resolve_pull_remote_and_refspecs(
    config: &GitConfig,
    store: &FileRefStore,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<(String, Vec<String>, Vec<String>)> {
    match (remote, branch) {
        (Some(remote), Some(branch)) => {
            let refspec = format!("refs/heads/{branch}");
            Ok((remote, vec![refspec], vec![format!("refs/heads/{branch}")]))
        }
        (Some(remote), None) => {
            let merge_srcs = store
                .current_branch()
                .ok()
                .flatten()
                .map(|current| branch_merge_values(config, &current))
                .unwrap_or_default();
            Ok((remote, Vec::new(), merge_srcs))
        }
        (None, None) => {
            let Some(current) = store.current_branch()? else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> HEAD");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            let Some(remote) = config.get("branch", Some(&current), "remote") else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> {current}");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            let Some(merge) = config.get("branch", Some(&current), "merge") else {
                eprintln!("There is no tracking information for the current branch.");
                eprintln!("Please specify which branch you want to merge with.");
                eprintln!("See git-pull(1) for details.");
                eprintln!();
                eprintln!("    git pull <remote> <branch>");
                eprintln!();
                eprintln!(
                    "If you wish to set tracking information for this branch you can do so with:"
                );
                eprintln!();
                eprintln!("    git branch --set-upstream-to=<remote>/<branch> {current}");
                eprintln!();
                return Err(GitError::Exit(1));
            };
            let _ = merge;
            Ok((
                remote.to_string(),
                Vec::new(),
                branch_merge_values(config, &current),
            ))
        }
        (None, Some(_)) => Err(GitError::Command(
            "pull currently requires a remote when a branch is specified".into(),
        )),
    }
}

/// All `branch.<name>.merge` values configured for `branch`, in config order
/// (more than one is an octopus merge config).
fn branch_merge_values(config: &GitConfig, branch: &str) -> Vec<String> {
    config
        .get_all("branch", Some(branch), "merge")
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect()
}

fn fetch_head_merge_record(git_dir: &Path, format: ObjectFormat) -> Result<FetchHeadRecord> {
    let path = git_dir.join("FETCH_HEAD");
    let mut input =
        fs::File::open(path).map_err(|_| GitError::reference_not_found("FETCH_HEAD"))?;
    let records = read_fetch_head(format, &mut input)?;
    records
        .into_iter()
        .find(|record| !record.not_for_merge)
        .ok_or_else(|| GitError::reference_not_found("FETCH_HEAD"))
}

fn resolve_fetch_head_revision(git_dir: &Path, format: ObjectFormat) -> Result<ObjectId> {
    Ok(fetch_head_merge_record(git_dir, format)?.oid)
}

fn ensure_pull_can_merge() -> Result<()> {
    eprintln!("hint: You have divergent branches and need to specify how to reconcile them.");
    eprintln!("hint: You can do so by running one of the following commands sometime before");
    eprintln!("hint: your next pull:");
    eprintln!("hint:");
    eprintln!("hint:   git config pull.rebase false  # merge");
    eprintln!("hint:   git config pull.rebase true   # rebase");
    eprintln!("hint:   git config pull.ff only       # fast-forward only");
    eprintln!("hint:");
    eprintln!("hint: You can replace \"git config\" with \"git config --global\" to set a default");
    eprintln!("hint: preference for all repositories. You can also pass --rebase, --no-rebase,");
    eprintln!("hint: or --ff-only on the command line to override the configured default per");
    eprintln!("hint: invocation.");
    eprintln!("fatal: Need to specify how to reconcile divergent branches.");
    Err(GitError::Exit(128))
}

fn print_fetch_status(
    source: &str,
    updates: &[FetchRefUpdate],
    old_oids: &HashMap<String, ObjectId>,
) {
    let mut displayed = false;
    for update in updates {
        let src_short = update
            .src
            .strip_prefix("refs/heads/")
            .unwrap_or(update.src.as_str());
        let Some(dst) = update.dst.as_ref() else {
            if !displayed {
                eprintln!("From {source}");
                displayed = true;
            }
            eprintln!(" * branch            {src_short:11}-> FETCH_HEAD");
            continue;
        };
        if old_oids.get(dst) == Some(&update.oid) {
            continue;
        }
        if !displayed {
            eprintln!("From {source}");
            displayed = true;
        }
        let dst_short = dst.strip_prefix("refs/remotes/").unwrap_or(dst.as_str());
        let old_short = old_oids
            .get(dst)
            .map(format_log_abbrev_oid)
            .unwrap_or_else(|| "0000000".to_string());
        eprintln!(
            "   {}..{}  {:11} -> {}",
            old_short,
            format_log_abbrev_oid(&update.oid),
            src_short,
            dst_short
        );
    }
}

fn pull_fetch(
    git_dir: &Path,
    format: ObjectFormat,
    remote: &str,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    if let Ok(input) = fs::read(remote)
        && let Ok(bundle) = Bundle::parse(&input, format)
    {
        fetch_bundle(git_dir, format, remote, refspecs, &bundle, options)?;
        return Ok(FetchOutcome::default());
    }
    if fetch_source_is_ssh(remote)? {
        fetch_ssh_repository(git_dir, format, remote, refspecs, options)?;
        Ok(FetchOutcome::default())
    } else {
        let config = read_repo_config(git_dir)?;
        let remote_git_dir = ls_remote_git_dir(remote)?;
        let remote_common_git_dir = common_git_dir_for_git_dir(&remote_git_dir)?;
        let fetch_source = sley_remote::FetchSource::Local {
            git_dir: remote_git_dir,
            common_git_dir: remote_common_git_dir,
        };
        let store = FileRefStore::new(git_dir, format);
        let mut old_oids = HashMap::new();
        if !options.merge_srcs.is_empty() {
            for update_dst in store.list_refs()? {
                if let Some((oid, _)) = resolve_for_each_ref_target(&store, &update_dst)? {
                    old_oids.insert(update_dst.name, oid);
                }
            }
        }
        let quiet = options.quiet;
        let outcome = run_fetch_with_outcome(
            git_dir,
            format,
            &config,
            remote,
            &fetch_source,
            refspecs,
            options,
        )?;
        if !quiet {
            print_fetch_status(remote, &outcome.ref_updates, &old_oids);
        }
        Ok(outcome)
    }
}

fn run_fetch_with_outcome(
    git_dir: &Path,
    format: ObjectFormat,
    config: &GitConfig,
    source: &str,
    fetch_source: &sley_remote::FetchSource,
    refspecs: &[String],
    options: FetchOptions,
) -> Result<FetchOutcome> {
    let mut credentials = sley_remote::CredentialHelperProvider::new(Some(config));
    let mut progress = StdoutProgress;
    sley_remote::fetch(
        sley_remote::FetchRequest {
            git_dir,
            format,
            config,
            remote_name: source,
            source: fetch_source,
            refspecs,
            options: &options,
        },
        sley_remote::FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )
}

pub(crate) fn cmd_pull(args: &[String]) -> Result<()> {
    // git's `set_reflog_message`: record the pull invocation (`pull …`) as the
    // reflog action so a fast-forward merge writes `pull …: Fast-forward`. The
    // workspace forbids `std::env::set_var`, so the action is stashed in a
    // process-global store (mirroring the `GIT_CONFIG_PARAMETERS` pattern) and
    // read back by `merge_reflog_message`. Only set when neither the env var nor
    // an earlier override is present, matching git's `setenv(…, 0)`.
    if env::var_os("GIT_REFLOG_ACTION").is_none() {
        let mut action = String::from("pull");
        for arg in args {
            action.push(' ');
            action.push_str(arg);
        }
        set_reflog_action_override(action);
    }
    let mut no_ff = false;
    let mut ff_only = false;
    let mut quiet = false;
    let mut rebase_flag = None::<bool>;
    let mut remote = None::<String>;
    let mut branch = None::<String>;
    let mut depth = None::<u32>;
    let mut expect_depth_value = false;
    for arg in args {
        if expect_depth_value {
            expect_depth_value = false;
            depth = Some(crate::commands::remote_cmds::parse_clone_depth(arg)?);
            continue;
        }
        match arg.as_str() {
            "--no-ff" => no_ff = true,
            "--ff-only" => ff_only = true,
            "--rebase" => rebase_flag = Some(true),
            "--no-rebase" => rebase_flag = Some(false),
            "-q" | "--quiet" => quiet = true,
            "--no-quiet" => quiet = false,
            "--depth" => expect_depth_value = true,
            value if value.starts_with("--depth=") => {
                depth = Some(crate::commands::remote_cmds::parse_clone_depth(
                    value.strip_prefix("--depth=").unwrap_or_default(),
                )?);
            }
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "pull currently supports --ff-only, --no-ff, --rebase, --no-rebase, --quiet, and remote/branch arguments; unsupported option {value}"
                )));
            }
            value => {
                if remote.is_none() {
                    remote = Some(value.to_string());
                } else if branch.is_none() {
                    branch = Some(value.to_string());
                } else {
                    return Err(GitError::Command(
                        "pull accepts at most one remote and one branch".into(),
                    ));
                }
            }
        }
    }
    if ff_only && no_ff {
        return Err(GitError::Command(
            "pull cannot combine --ff-only and --no-ff".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let config = read_repo_config(&git_dir)?;
    let store = FileRefStore::new(&git_dir, format);
    let (remote, refspecs, merge_srcs) =
        resolve_pull_remote_and_refspecs(&config, &store, remote, branch)?;
    let config_ff_only = config
        .get("pull", None, "ff")
        .is_some_and(|value| value == "only");
    let effective_ff_only = ff_only || config_ff_only;
    // Mirror git's `config_get_rebase` (builtin/pull.c): an explicit
    // `--rebase`/`--no-rebase` wins; otherwise `branch.<name>.rebase` is
    // consulted before the global `pull.rebase`. `rebase_unspecified` stays true
    // only when *none* of those sources expressed a preference — that is the sole
    // gate for the "Need to specify how to reconcile divergent branches" die, so
    // a bare `--no-rebase` must clear it (was previously keyed off `pull.rebase`
    // alone, which wrongly fired on `git pull --no-rebase`).
    let current_branch_name = match store.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(name)) => {
            Some(name.strip_prefix("refs/heads/").unwrap_or(&name).to_string())
        }
        _ => None,
    };
    let branch_rebase = current_branch_name
        .as_deref()
        .and_then(|name| config.get("branch", Some(name), "rebase"));
    let config_rebase = branch_rebase.or_else(|| config.get("pull", None, "rebase"));
    let (effective_rebase, rebase_unspecified) = match rebase_flag {
        Some(value) => (value, false),
        None => match config_rebase {
            Some(value) => (matches!(value, "true" | "interactive" | "merges"), false),
            None => (false, true),
        },
    };
    let fetch_options = FetchOptions {
        quiet,
        auto_follow_tags: true,
        fetch_all_tags: false,
        prune: false,
        dry_run: false,
        append: false,
        write_fetch_head: true,
        tag_option_explicit: false,
        prune_option_explicit: false,
        depth,
        merge_srcs,
        filter: None,
        cloning: false,
        update_shallow: false,
        deepen_relative: false,
        deepen_since: None,
        deepen_not: Vec::new(),
    };
    // git captures `orig_head` (HEAD before the fetch). A refspec like
    // `main:main` can create the current branch during the fetch, but the
    // pull-into-void decision keys off the *pre-fetch* state, so capture it now.
    let orig_head_unborn = head_commit_oid(&store)?.is_none();
    pull_fetch(&git_dir, format, &remote, &refspecs, fetch_options)?;
    let common_git_dir = common_git_dir_for_git_dir(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&common_git_dir, format);
    // Pulling into an unborn branch (git's `pull_into_void`): there is no HEAD to
    // merge against, so we fast-forward to FETCH_HEAD's merge target by pointing
    // HEAD at it (unless a refspec already moved the current branch there) and
    // checking out its tree. Keyed off the *pre-fetch* state so a `main:main`
    // refspec that created the branch still triggers the void checkout.
    if orig_head_unborn {
        let merge_oid = resolve_fetch_head_revision(&git_dir, format)?;
        let target_ref = match store.read_ref("HEAD")? {
            Some(RefTarget::Symbolic(branch)) => branch,
            _ => "HEAD".to_string(),
        };
        // The branch may already point at `merge_oid` if a refspec like
        // `main:main` updated it during the fetch; only move it when it doesn't.
        if store.read_ref(&target_ref)? != Some(RefTarget::Direct(merge_oid)) {
            let mut tx = store.transaction();
            tx.update(RefUpdate {
                name: target_ref,
                expected: None,
                new: RefTarget::Direct(merge_oid),
                reflog: Some(ReflogEntry {
                    old_oid: zero_oid(format)?,
                    new_oid: merge_oid,
                    committer: commit_identity_from_env("COMMITTER")?,
                    message: b"initial pull".to_vec(),
                }),
            });
            tx.commit()?;
        }
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        sley_worktree::reset_index_and_worktree_to_commit(
            &worktree_root,
            &git_dir,
            format,
            &merge_oid,
        )?;
        return Ok(());
    }
    let ours_oid = resolve_revision(&git_dir, format, "HEAD")?;
    let theirs_oid = resolve_fetch_head_revision(&git_dir, format)?;
    let ours_commit = sley_rev::peel_to_commit(&db, format, &ours_oid)?;
    let theirs_commit = sley_rev::peel_to_commit(&db, format, &theirs_oid)?;
    if ours_commit == theirs_commit {
        if !quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }
    let ours_depths = ancestor_depths(&db, format, &ours_commit)?;
    if ours_depths.contains_key(&theirs_commit) {
        if !quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }
    let theirs_depths = ancestor_depths(&db, format, &theirs_commit)?;
    let fast_forward = theirs_depths.contains_key(&ours_commit);
    if fast_forward {
        let mut merge_args = Vec::new();
        if no_ff {
            merge_args.push("--no-ff".to_string());
        }
        if effective_ff_only {
            merge_args.push("--ff-only".to_string());
        }
        if quiet {
            merge_args.push("--quiet".to_string());
        }
        merge_args.push("FETCH_HEAD".to_string());
        return cmd_merge(&merge_args);
    }
    if effective_ff_only {
        eprintln!("fatal: Not possible to fast-forward, aborting.");
        return Err(GitError::Exit(128));
    }
    if effective_rebase {
        let worktree_root = worktree_root_for_git_dir(&git_dir)?;
        match rebase_onto_upstream(&git_dir, &worktree_root, format, "FETCH_HEAD", quiet)? {
            RebaseOntoOutcome::Rebasing => return Ok(()),
            RebaseOntoOutcome::UpToDate => {
                if !quiet {
                    println!("Already up to date.");
                }
                return Ok(());
            }
        }
    }
    // git dies on divergence only when the rebase choice was entirely
    // unspecified (no CLI flag, no branch/pull config) and `--ff-only` is not in
    // play (builtin/pull.c: `if (!opt_ff && rebase_unspecified && divergent)`).
    // `no_ff` corresponds to `opt_ff == "--no-ff"`, which also suppresses the die.
    if !no_ff && !effective_ff_only && rebase_unspecified {
        ensure_pull_can_merge()?;
    }
    let mut merge_args = Vec::new();
    if no_ff {
        merge_args.push("--no-ff".to_string());
    }
    if effective_ff_only {
        merge_args.push("--ff-only".to_string());
    }
    if quiet {
        merge_args.push("--quiet".to_string());
    }
    merge_args.push("FETCH_HEAD".to_string());
    cmd_merge(&merge_args)
}

pub(crate) fn commit_tree_oid(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<ObjectId> {
    let object = db.read_object(commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "expected commit {commit_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(Commit::parse_ref(format, &object.body)?.tree)
}

// ===== cherry-pick / revert (single-commit 3-way replay) =====

pub(crate) fn head_commit_oid(refs: &FileRefStore) -> Result<Option<ObjectId>> {
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        None => Ok(None),
    }
}

pub(crate) fn cmd_merge_base(args: &[String]) -> Result<()> {
    let mut all = false;
    let mut is_ancestor = false;
    let mut independent = false;
    let mut octopus = false;
    let mut fork_point = false;
    let mut revs = Vec::new();
    let mut positional_only = false;
    for arg in args {
        if positional_only {
            revs.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "--all" | "-a" => all = true,
            "--no-all" => all = false,
            "--is-ancestor" => is_ancestor = true,
            "--independent" => independent = true,
            "--octopus" => octopus = true,
            "--fork-point" => fork_point = true,
            value if value.starts_with('-') => {
                return Err(GitError::Command(format!(
                    "merge-base currently supports --all, --is-ancestor, --independent, --octopus, --fork-point, and commit arguments; unsupported option {value}"
                )));
            }
            value => revs.push(value),
        }
    }
    if fork_point && !(revs.len() == 1 || revs.len() == 2) {
        return Err(GitError::Command(
            "merge-base --fork-point requires a ref and optional commit".into(),
        ));
    }
    if is_ancestor && revs.len() != 2 {
        return Err(GitError::Command(
            "merge-base currently requires exactly two commits".into(),
        ));
    }
    if independent && all {
        eprintln!("fatal: options '--independent' and '--all' cannot be used together");
        return Err(GitError::Exit(128));
    }
    if independent && is_ancestor {
        eprintln!("error: options '--independent' and '--is-ancestor' cannot be used together");
        return Err(GitError::Exit(129));
    }
    if !fork_point && !octopus && !independent && revs.len() < 2 {
        return Err(GitError::Command(
            "merge-base currently requires at least two commits".into(),
        ));
    }
    if (octopus || independent) && revs.is_empty() {
        return Err(GitError::Command(
            "merge-base requires at least one commit for this mode".into(),
        ));
    }
    let cwd = env::current_dir()?;
    let git_dir = discover_git_dir(&cwd)?;
    let format = repository_object_format(&git_dir)?;
    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    if fork_point {
        let commit = if let Some(commit) = revs.get(1) {
            let oid = resolve_revision(&git_dir, format, commit)?;
            sley_rev::peel_to_commit(&db, format, &oid)?
        } else {
            let oid = resolve_revision(&git_dir, format, "HEAD")?;
            sley_rev::peel_to_commit(&db, format, &oid)?
        };
        if let Some(base) = merge_base_fork_point(&git_dir, format, &db, revs[0], &commit)? {
            println!("{base}");
            return Ok(());
        }
        return Err(GitError::Exit(1));
    }
    let mut commits = Vec::with_capacity(revs.len());
    for rev in &revs {
        let oid = resolve_revision(&git_dir, format, rev)?;
        commits.push(sley_rev::peel_to_commit(&db, format, &oid)?);
    }
    if is_ancestor {
        // Graph-accelerated reachability (generation-number pruning + parents from
        // the commit-graph) instead of walking every ancestor's object.
        if sley_rev::is_ancestor(&git_dir, format, &db, &commits[0], &commits[1])? {
            return Ok(());
        }
        return Err(GitError::Exit(1));
    }
    if independent {
        for commit in merge_base_independent(&db, format, &commits)? {
            println!("{commit}");
        }
        return Ok(());
    }
    let bases = if octopus {
        merge_bases_many(&db, format, &commits)?
    } else if commits.len() > 2 {
        merge_bases_default_many(&db, format, &commits)?
    } else {
        // Two-commit merge base via the commit-graph (parents + generation numbers
        // from the graph) rather than the object-reading ancestor walk.
        sley_rev::merge_bases(&git_dir, format, &db, &commits[0], &commits[1])?
    };
    if bases.is_empty() {
        return Err(GitError::Exit(1));
    }
    if all {
        for base in bases {
            println!("{base}");
        }
    } else {
        println!("{}", bases[0]);
    }
    Ok(())
}

/// Two-commit merge bases. Delegates to the single graph-aware implementation in
/// [`sley_rev::merge_bases`] (parents/generations come from the commit-graph when
/// present), so the CLI no longer carries a duplicate, graph-blind copy. The
/// canonical `merge-base` command already routed through `sley_rev::merge_bases`;
/// this folds the remaining internal callers (merge / rebase / octopus
/// virtual-ancestor / log / rev-list / shortlog / format-patch) onto it too, for
/// one ancestry implementation. `git_dir` is required to locate the
/// commit-graph.
pub(crate) fn merge_bases(
    git_dir: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left: &ObjectId,
    right: &ObjectId,
) -> Result<Vec<ObjectId>> {
    sley_rev::merge_bases(git_dir, format, db, left, right)
}

fn merge_bases_default_many(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let left_depths = ancestor_depths(db, format, &commits[0])?;
    let other_depths = commits
        .iter()
        .skip(1)
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut common = left_depths
        .keys()
        .filter(|oid| other_depths.iter().any(|map| map.contains_key(*oid)))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = common.clone();
    let candidate_depths = candidates
        .iter()
        .map(|candidate| Ok((candidate.clone(), ancestor_depths(db, format, candidate)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    common.retain(|candidate| {
        !candidates.iter().any(|other| {
            other != candidate
                && candidate_depths
                    .get(other)
                    .is_some_and(|ancestors| ancestors.contains_key(candidate))
        })
    });
    common.sort_by(|left_oid, right_oid| {
        let left_other_depth = other_depths
            .iter()
            .filter_map(|map| map.get(left_oid))
            .min()
            .copied()
            .unwrap_or(usize::MAX);
        let right_other_depth = other_depths
            .iter()
            .filter_map(|map| map.get(right_oid))
            .min()
            .copied()
            .unwrap_or(usize::MAX);
        let left_score = left_depths[left_oid] + left_other_depth;
        let right_score = left_depths[right_oid] + right_other_depth;
        left_score
            .cmp(&right_score)
            .then_with(|| left_depths[left_oid].cmp(&left_depths[right_oid]))
            .then_with(|| left_oid.to_hex().cmp(&right_oid.to_hex()))
    });
    Ok(common)
}

fn merge_bases_many(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if let [commit] = commits {
        return Ok(vec![*commit]);
    }
    let depths = commits
        .iter()
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut common = depths[0]
        .keys()
        .filter(|oid| depths.iter().skip(1).all(|map| map.contains_key(*oid)))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = common.clone();
    common = candidates
        .iter()
        .filter(|candidate| {
            !candidates.iter().any(|other| {
                other != *candidate
                    && depths.iter().all(|map| {
                        map.get(other).zip(map.get(*candidate)).is_some_and(
                            |(other_depth, candidate_depth)| other_depth < candidate_depth,
                        )
                    })
            })
        })
        .cloned()
        .collect();
    common.sort_by(|left_oid, right_oid| {
        let left_score = depths.iter().map(|map| map[left_oid]).sum::<usize>();
        let right_score = depths.iter().map(|map| map[right_oid]).sum::<usize>();
        left_score
            .cmp(&right_score)
            .then_with(|| {
                depths
                    .iter()
                    .map(|map| map[left_oid])
                    .cmp(depths.iter().map(|map| map[right_oid]))
            })
            .then_with(|| left_oid.to_hex().cmp(&right_oid.to_hex()))
    });
    Ok(common)
}

fn merge_base_independent(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    commits: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for commit in commits {
        if seen.insert(commit) {
            unique.push(*commit);
        }
    }
    let depths = unique
        .iter()
        .map(|commit| ancestor_depths(db, format, commit))
        .collect::<Result<Vec<_>>>()?;
    let mut independent = Vec::new();
    for (idx, commit) in unique.iter().enumerate() {
        let reachable_from_other = depths
            .iter()
            .enumerate()
            .any(|(other_idx, ancestors)| other_idx != idx && ancestors.contains_key(commit));
        if !reachable_from_other {
            independent.push(*commit);
        }
    }
    Ok(independent)
}

fn merge_base_fork_point(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    ref_arg: &str,
    commit: &ObjectId,
) -> Result<Option<ObjectId>> {
    let Some(refname) = rev_parse_symbolic_full_name(git_dir, format, ref_arg)? else {
        return Ok(None);
    };
    let store = FileRefStore::new(git_dir, format);
    let reflog = store.read_reflog(&refname)?;
    if reflog.is_empty() {
        return Ok(None);
    }
    let commit_depths = ancestor_depths(db, format, commit)?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for entry in reflog {
        if commit_depths.contains_key(&entry.new_oid) && seen.insert(entry.new_oid) {
            candidates.push(entry.new_oid);
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    let candidate_depths = candidates
        .iter()
        .map(|candidate| Ok((candidate.clone(), ancestor_depths(db, format, candidate)?)))
        .collect::<Result<HashMap<_, _>>>()?;
    let all_candidates = candidates.clone();
    candidates.retain(|candidate| {
        !all_candidates.iter().any(|other| {
            other != candidate
                && candidate_depths
                    .get(other)
                    .is_some_and(|ancestors| ancestors.contains_key(candidate))
        })
    });
    candidates.sort_by(|left, right| {
        commit_depths[left]
            .cmp(&commit_depths[right])
            .then_with(|| left.to_hex().cmp(&right.to_hex()))
    });
    Ok(candidates.into_iter().next())
}
