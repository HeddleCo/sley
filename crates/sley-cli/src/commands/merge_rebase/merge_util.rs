use super::*;

// ===== git merge (3-way) =====

pub(crate) type MergeTreeMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

pub(crate) fn merge_read_blob(db: &FileObjectDatabase, oid: &ObjectId) -> Result<Vec<u8>> {
    let object = crate::read_object_maybe_prefetch_promisor(db, oid)?;
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

pub(crate) fn merge_worktree_content(
    db: &FileObjectDatabase,
    mode: u32,
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    if sley_index::is_gitlink(mode) {
        Ok(Vec::new())
    } else {
        merge_read_blob(db, oid)
    }
}

fn prefetch_content_merge_blobs(
    db: &FileObjectDatabase,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
) -> Result<()> {
    let mut paths = BTreeSet::new();
    paths.extend(base.keys().cloned());
    paths.extend(ours.keys().cloned());
    paths.extend(theirs.keys().cloned());

    for path in paths {
        let base_entry = base.get(&path);
        let ours_entry = ours.get(&path);
        let theirs_entry = theirs.get(&path);
        if ours_entry == theirs_entry || ours_entry == base_entry || theirs_entry == base_entry {
            continue;
        }
        let content_mergeable = ours_entry
            .is_some_and(|(mode, _)| sley_diff_merge::is_mergeable_file_mode(*mode))
            && theirs_entry.is_some_and(|(mode, _)| sley_diff_merge::is_mergeable_file_mode(*mode))
            && base_entry
                .map(|(mode, _)| sley_diff_merge::is_mergeable_file_mode(*mode))
                .unwrap_or(true);
        if !content_mergeable {
            continue;
        }
        for (_, oid) in [base_entry, ours_entry, theirs_entry].into_iter().flatten() {
            let _ = merge_read_blob(db, oid)?;
        }
    }
    Ok(())
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
    if sley_index::is_gitlink(mode) {
        // Gitlink (submodule) entry: the `oid` is a *commit*, not a blob, so it
        // must NOT be written as file content (the prior unconditional blob write
        // produced an "Is a directory"/garbage-content failure that gated the
        // revert/cherry-pick-over-submodule worktree apply, e.g.
        // create_lib_submodule_repo's `git revert HEAD`). git's entry.c
        // `write_entry` S_IFGITLINK arm only `mkdir`s the submodule directory
        // (`submodule_move_head` — the embedded checkout — is a higher layer sley
        // does not perform), preserving an already-populated submodule checkout.
        if full.is_dir() {
            return Ok(());
        }
        merge_unlink_path_in_the_way(&full)?;
        fs::create_dir_all(&full)?;
        return Ok(());
    }
    // Unlink whatever is in the way first (git's entry.c `write_entry`), so a type
    // change (regular file ⇄ symlink) is overwritten rather than written *through*
    // an existing symlink or left stale — the symlink-stash-apply / merge cases.
    merge_unlink_path_in_the_way(&full)?;
    if (mode & 0o170000) == 0o120000 {
        // Symlink entry (mode 120000): the blob bytes are the link target.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let target = std::path::PathBuf::from(std::ffi::OsString::from_vec(content.to_vec()));
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
                if merge_path_is_original_cwd(full) {
                    return merge_refuse_remove_current_working_directory(full);
                }
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
pub(crate) fn clear_merge_df_blockers(worktree_root: &Path, results: &MergePathResults) {
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

pub(crate) fn merge_remove_worktree_file(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    // lstat (symlink_metadata): `Path::exists` follows symlinks and misses a
    // dangling one, leaving it behind on removal.
    match fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_dir() => {
            if merge_path_is_original_cwd(&full) {
                return Ok(());
            }
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
        // ENOTDIR: a path component is a (non-directory) file, so the target
        // cannot exist — it was already removed (e.g. a directory→file typechange
        // cleared the parent before this delete ran). git's `unlink_or_warn`
        // treats this as already-gone; mirror that.
        Err(err) if err.raw_os_error() == Some(20) => {}
        Err(err) => return Err(err.into()),
    }
    merge_prune_empty_dirs(worktree_root, full.parent());
    Ok(())
}

pub(crate) fn merge_refuse_if_current_working_directory_becomes_file(
    worktree_root: &Path,
    target_entries: &MergeTreeMap,
) -> Result<()> {
    let Some(cwd) = merge_original_cwd_relative_to(worktree_root) else {
        return Ok(());
    };
    if target_entries.iter().any(|(path, (mode, _))| {
        path == &cwd && !sley_index::is_gitlink(*mode) && (mode & 0o170000) != 0o040000
    }) {
        let full = worktree_root.join(path_from_git_bytes_lossy(&cwd));
        if fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_dir()) {
            return merge_refuse_remove_current_working_directory(&full);
        }
    }
    Ok(())
}

fn merge_original_cwd_absolute() -> Option<PathBuf> {
    let cwd = sley_core::original_cwd().or_else(|| env::current_dir().ok())?;
    Some(fs::canonicalize(&cwd).unwrap_or(cwd))
}

fn merge_original_cwd_relative_to(worktree_root: &Path) -> Option<Vec<u8>> {
    let root = fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let cwd = merge_original_cwd_absolute()?;
    if cwd == root {
        return None;
    }
    let rel = cwd.strip_prefix(&root).ok()?;
    Some(path_to_git_bytes_lossy(rel))
}

fn merge_path_is_original_cwd(path: &Path) -> bool {
    let Some(cwd) = merge_original_cwd_absolute() else {
        return false;
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path == cwd
}

fn merge_refuse_remove_current_working_directory(path: &Path) -> Result<()> {
    eprintln!(
        "error: Refusing to remove the current working directory:\n{}",
        path.display()
    );
    Err(GitError::Exit(128))
}

fn merge_prune_empty_dirs(root: &Path, mut dir: Option<&Path>) {
    while let Some(path) = dir {
        if path == root || merge_path_is_original_cwd(path) {
            break;
        }
        if fs::remove_dir(path).is_err() {
            break;
        }
        dir = path.parent();
    }
}

fn path_to_git_bytes_lossy(path: &Path) -> Vec<u8> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes()
}

fn path_from_git_bytes_lossy(path: &[u8]) -> PathBuf {
    String::from_utf8_lossy(path).split('/').collect()
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

pub(crate) type MergePathResults = BTreeMap<Vec<u8>, MergePathResult>;
pub(crate) type MergeConflictPaths = Vec<Vec<u8>>;
pub(crate) type MergeInfoMessages = Vec<sley_diff_merge::MergeInfoMessage>;
pub(crate) type MergePathFavorResolver<'a> = dyn Fn(&[u8]) -> sley_diff_merge::MergeFavor + 'a;

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
pub(crate) fn virtual_ancestor_entry_map(
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
                    if let Some((mode, _)) = worktree
                        && sley_index::is_gitlink(mode)
                    {
                        if let Some(entry) = theirs.or(ours) {
                            next.insert(path, entry);
                        }
                    } else if let Some((mode, bytes)) = worktree {
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
    let (results, conflicts, _) = three_way_merge_trees_inner_with_info(
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
    )?;
    Ok((results, conflicts))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn three_way_merge_trees_inner_with_info(
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
            rename_limit: merge_rename_limit_config(),
            directory_renames: directory_renames_config(),
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
    // The shared merge engine only sees an object database, while the CLI knows
    // how to hydrate promised blobs. Fetch just the blobs a non-trivial textual
    // merge will inspect, preserving partial-clone laziness for unrelated blobs.
    prefetch_content_merge_blobs(db, base, ours, theirs)?;

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
            path_favor,
            detect_renames: renames.detect_renames,
            rename_threshold: renames.rename_threshold,
            rename_limit: renames.rename_limit,
            // Directory-rename detection only fires when file-rename detection is
            // enabled (it is inferred from the file renames found). With renames
            // off, force it off too so `--no-renames` disables both.
            directory_renames: if renames.detect_renames {
                renames.directory_renames
            } else {
                sley_diff_merge::DirectoryRenames::False
            },
            style,
            ws_ignore,
        },
    )?;

    let mut results = BTreeMap::new();
    let mut conflicts = Vec::new();
    let info_messages = merge.info_messages.clone();
    let cleanup_paths = merge.cleanup_paths;
    for entry in merge.paths {
        // A directory-rename location "conflict" (=conflict mode) is purely
        // advisory: git stages the re-homed content cleanly at stage 0 and only
        // emits an informational `CONFLICT (file location)` message + nonzero
        // exit. Carry the resolved leaf in the `ours` stage slot and rely on the
        // index/worktree writers to stage `DirRenameLocation` at stage 0.
        let advisory_location = matches!(
            entry.conflict,
            Some(sley_diff_merge::MergeConflictKind::DirRenameLocation {
                back_to_self: false,
                ..
            }) | Some(sley_diff_merge::MergeConflictKind::DirRenameImplicitCollision { .. })
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
    for path in cleanup_paths {
        results
            .entry(path)
            .or_insert(MergePathResult::Resolved(None));
    }
    Ok((results, conflicts, info_messages))
}
