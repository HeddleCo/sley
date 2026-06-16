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
        fs::create_dir_all(parent)?;
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
        if entry.conflict.is_some() {
            conflicts.push(entry.path.clone());
            results.insert(
                entry.path,
                MergePathResult::Conflict {
                    base: entry.stages.base,
                    ours: entry.stages.ours,
                    theirs: entry.stages.theirs,
                    worktree: entry.worktree,
                },
            );
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
    if options.no_commit || options.ff_only {
        return Err(GitError::Unsupported(
            "octopus merges with --no-commit or --ff-only are not supported yet".into(),
        ));
    }
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

    let mut heads = Vec::with_capacity(targets.len());
    for target in targets {
        let oid = resolve_revision(git_dir, format, target)?;
        let commit = sley_rev::peel_to_commit(&db, format, &oid)?;
        heads.push((target.clone(), commit));
    }

    // git's `reduce_heads`: drop heads already reachable from HEAD or from
    // another head (a duplicate keeps only its first occurrence).
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
        if is_ancestor(&db, oid, &head_oid)? {
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
    for (name, oid) in &reduced {
        let mut base_args = vec![*oid];
        base_args.extend(merged_commits.iter().copied());
        let common = merge_bases_default_many(&db, format, &base_args)?;
        if common.len() == 1 && common[0] == *oid {
            // Already covered by the merges performed so far.
            continue;
        }
        if !non_ff
            && merged_commits.len() == 1
            && common.len() == 1
            && common[0] == merged_commits[0]
        {
            // Fast-forward the running state to this head.
            let tree = commit_tree_oid(&db, format, oid)?;
            merged_map = stash_tree_entry_map(&db, format, &tree)?;
            merged_commits = vec![*oid];
            continue;
        }
        if common.is_empty() {
            eprintln!("Unable to find common commit with {name}");
            return Err(GitError::Exit(2));
        }
        non_ff = true;
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
            // Octopus refuses to leave conflicts for manual resolution.
            eprintln!("Simple merge did not work, octopus merge is not possible.");
            eprintln!("Merge with strategy octopus failed.");
            return Err(GitError::Exit(2));
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

    let message = options.message.clone().unwrap_or_else(|| {
        let names = reduced
            .iter()
            .map(|(name, _)| format!("'{name}'"))
            .collect::<Vec<_>>();
        let list = match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => names.join(", "),
        };
        format!("Merge branches {list}")
    });

    if !options.quiet {
        let mut stdout = io::stdout();
        writeln!(stdout, "Merge made by the 'octopus' strategy.")?;
        write_merge_result_diffstat(&mut stdout, &db, format, &head_tree, &merged_tree, merge_diffstat_mode(options))?;
        stdout.flush()?;
    }

    let author = commit_identity_from_env("AUTHOR")?;
    let committer = commit_identity_from_env("COMMITTER")?;
    let mut write_db = FileObjectDatabase::from_git_dir(common_git_dir, format);
    let mut parents = vec![head_oid];
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
                CommitCleanupMode::Whitespace,
                "#",
                false,
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
    // Mark HEAD's ancestors uninteresting, then collect `other`'s ancestors that
    // are not among them (the `^HEAD other` range).
    let uninteresting = ancestor_depths(db, format, head)?;
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([other.clone()]);
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

/// The default merge commit subject for a single-parent merge of `target`,
/// mirroring git's `merge_name` + `fmt_merge_msg`: dwim the target to a ref and
/// pick `Merge tag '<n>'` / `Merge branch '<n>'` / `Merge remote-tracking branch
/// '<n>'` / `Merge commit '<n>'`. Precedence follows git's `ref_rev_parse_rules`
/// (tags before heads), so a tag wins a name it shares with a branch. FETCH_HEAD
/// keeps its own fetch-record-derived description.
fn merge_default_message(
    refs: &FileRefStore,
    git_dir: &Path,
    format: ObjectFormat,
    target: &str,
) -> Result<String> {
    if target == "FETCH_HEAD" {
        return Ok(fetch_head_merge_record(git_dir, format)
            .map(|record| format!("Merge {}", record.description))
            .unwrap_or_else(|_| format!("Merge commit '{target}'")));
    }
    let exists = |name: &str| -> Result<bool> { Ok(refs.read_ref(name)?.is_some()) };
    if exists(&format!("refs/tags/{target}"))? {
        Ok(format!("Merge tag '{target}'"))
    } else if exists(&format!("refs/heads/{target}"))? {
        Ok(format!("Merge branch '{target}'"))
    } else if exists(&format!("refs/remotes/{target}"))? {
        Ok(format!("Merge remote-tracking branch '{target}'"))
    } else {
        Ok(format!("Merge commit '{target}'"))
    }
}

struct MergeOptions {
    message: Option<String>,
    no_ff: bool,
    ff_only: bool,
    no_commit: bool,
    quiet: bool,
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
            no_ff: false,
            ff_only: false,
            no_commit: false,
            quiet: false,
            favor: sley_diff_merge::MergeFavor::None,
            allow_unrelated_histories: false,
            diffstat: None,
            ours_strategy: false,
            squash: false,
        }
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
    continue_merge: bool,
    positional: Vec<String>,
}

fn set_merge_fast_forward(options: &mut MergeOptions, no_ff: bool, ff_only: bool) {
    options.no_ff = no_ff;
    options.ff_only = ff_only;
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
            "--continue" => parsed.continue_merge = true,
            "--no-ff" => set_merge_fast_forward(options, true, false),
            "--ff" => set_merge_fast_forward(options, false, false),
            "--ff-only" => set_merge_fast_forward(options, false, true),
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
    if continue_merge {
        if !positional.is_empty() || options.no_ff || options.ff_only || options.message.is_some() {
            eprintln!("fatal: --continue expects no arguments");
            return Err(GitError::Exit(129));
        }
        return cmd_merge_continue();
    }

    if git_dir.join("MERGE_HEAD").exists() {
        return Err(GitError::Command(
            "You have not concluded your merge (MERGE_HEAD exists).".into(),
        ));
    }

    let target = match positional.as_slice() {
        [target] => target.clone(),
        [] => {
            return Err(GitError::Command("merge requires a commit argument".into()));
        }
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
        if options.ff_only {
            eprintln!("fatal: Not possible to fast-forward, aborting.");
            return Err(GitError::Exit(128));
        }
        fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;
        let head_tree = commit_tree_oid(&db, format, &head_oid)?;
        let default_message = merge_default_message(&refs, &git_dir, format, &target)?;
        let message = options.message.clone().unwrap_or(default_message);
        if options.no_commit {
            fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
            fs::write(git_dir.join("MERGE_MSG"), format!("{message}\n"))?;
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
                CommitCleanupMode::Whitespace,
                "#",
                false,
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

    if can_fast_forward && !options.no_ff {
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

    if options.ff_only {
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

    let default_message = merge_default_message(&refs, &git_dir, format, &target)?;
    let message = options.message.clone().unwrap_or(default_message);

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
        // --squash and --no-commit early-exit paths below).
        let write_merged_worktree = || -> Result<()> {
            for (path, result) in &results {
                if let MergePathResult::Resolved(value) = result {
                    match value {
                        Some((mode, oid)) => {
                            let content = merge_read_blob(&db, oid)?;
                            merge_write_worktree_file(&worktree_root, path, &content, *mode)?;
                        }
                        None => merge_remove_worktree_file(&worktree_root, path)?,
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
                CommitCleanupMode::Whitespace,
                "#",
                false,
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

    // Conflicted merge: write a staged index, materialize worktree, record state.
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

    fs::write(git_dir.join("MERGE_HEAD"), format!("{other_oid}\n"))?;
    let mut merge_msg = format!("{message}\n\n# Conflicts:\n");
    for path in &conflicts {
        merge_msg.push_str(&format!("#\t{}\n", String::from_utf8_lossy(path)));
    }
    fs::write(git_dir.join("MERGE_MSG"), merge_msg)?;
    fs::write(git_dir.join("ORIG_HEAD"), format!("{head_oid}\n"))?;

    for path in &conflicts {
        println!("Auto-merging {}", String::from_utf8_lossy(path));
        println!(
            "CONFLICT (content): Merge conflict in {}",
            String::from_utf8_lossy(path)
        );
    }
    eprintln!("Automatic merge failed; fix conflicts and then commit the result.");
    Err(GitError::Exit(1))
}

// ===== pull / rebase / merge-continue =====
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
    let orig_head_path = git_dir.join("ORIG_HEAD");
    let target_oid = if orig_head_path.is_file() {
        let contents = fs::read_to_string(&orig_head_path)?;
        ObjectId::from_hex(format, contents.trim()).map_err(|_| {
            GitError::InvalidObject(format!("invalid ORIG_HEAD value {}", contents.trim()))
        })?
    } else {
        resolve_revision(&git_dir, format, "HEAD")?
    };

    let db = FileObjectDatabase::from_git_dir(&git_dir, format);
    let old_head = resolve_revision(&git_dir, format, "HEAD")?;
    let target_commit = sley_rev::peel_to_commit(&db, format, &target_oid)?;
    sley_worktree::reset_index_and_worktree_to_commit(
        &worktree_root,
        &git_dir,
        format,
        &target_commit,
    )?;
    update_reset_head_ref(
        &git_dir,
        format,
        old_head,
        target_commit,
        "HEAD",
        commit_identity_from_env("COMMITTER")?,
    )?;

    clear_in_progress_merge_state(&git_dir);
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

    let status = sley_worktree::short_status(worktree_root, git_dir, format)?;
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

fn ensure_pull_can_merge(config: &GitConfig) -> Result<()> {
    if config.get("pull", None, "rebase").is_none() {
        eprintln!("hint: You have divergent branches and need to specify how to reconcile them.");
        eprintln!("hint: You can do so by running one of the following commands sometime before");
        eprintln!("hint: your next pull:");
        eprintln!("hint:");
        eprintln!("hint:   git config pull.rebase false  # merge");
        eprintln!("hint:   git config pull.rebase true   # rebase");
        eprintln!("hint:   git config pull.ff only       # fast-forward only");
        eprintln!("hint:");
        eprintln!(
            "hint: You can replace \"git config\" with \"git config --global\" to set a default"
        );
        eprintln!(
            "hint: preference for all repositories. You can also pass --rebase, --no-rebase,"
        );
        eprintln!("hint: or --ff-only on the command line to override the configured default per");
        eprintln!("hint: invocation.");
        eprintln!("fatal: Need to specify how to reconcile divergent branches.");
        return Err(GitError::Exit(128));
    }
    Ok(())
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
    let effective_rebase = match rebase_flag {
        Some(value) => value,
        None => config.get("pull", None, "rebase") == Some("true"),
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
    ensure_pull_can_merge(&config)?;
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
