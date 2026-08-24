//! The merge/apply backend shared by the sequencer drive loops (cherry-pick,
//! revert, rebase --merge).
//!
//! Stage-B1 relocation: the flattened-tree merge adapter, the index/worktree
//! appliers, and the strategy-option plumbing previously living in the CLI's
//! `commands/merge_rebase/merge_util.rs`. Implementations are unchanged — only
//! their home moved so the sequencer engines own repo semantics while the CLI
//! keeps argv parsing and rendering. The CLI module re-exports these under the
//! historical `commands::merge_rebase::*` paths.
//!
//! Partial-clone laziness is injected: library crates cannot spawn the
//! promisor fetch, so blob reads go through an optional
//! [`PromisorObjectFetch`] supplied by the host (the CLI passes its
//! lazy-fetch adapter when enabled).

use sley_config::GitConfig;
use sley_core::{BString, GitError, ObjectFormat, ObjectId, Result};
use sley_diff_merge as dm;
use sley_index::{IndexEntry, is_gitlink};
use sley_object::{Commit, EncodedObject, ObjectType};
use sley_odb::{FileObjectDatabase, ObjectReader};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ===== blob access =====

/// Host-injected partial-clone hydration (git's promisor fetch). The CLI
/// adapter spawns the configured remote fetch; without one, reads are plain
/// ODB lookups and a missing object surfaces as an error like a full clone.
pub trait PromisorObjectFetch {
    fn read_object_maybe_prefetch(
        &self,
        db: &FileObjectDatabase,
        oid: &ObjectId,
    ) -> Result<Arc<EncodedObject>>;
}

pub fn merge_read_blob_with_fetch(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    fetch: Option<&dyn PromisorObjectFetch>,
) -> Result<Vec<u8>> {
    let object = match fetch {
        Some(fetch) => fetch.read_object_maybe_prefetch(db, oid)?,
        None => db.read_object(oid)?,
    };
    if object.object_type != ObjectType::Blob {
        return Err(GitError::InvalidObject(format!(
            "expected blob {oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Ok(object.body.clone())
}

pub fn merge_worktree_content(
    db: &FileObjectDatabase,
    mode: u32,
    oid: &ObjectId,
    fetch: Option<&dyn PromisorObjectFetch>,
) -> Result<Vec<u8>> {
    if is_gitlink(mode) {
        Ok(Vec::new())
    } else {
        merge_read_blob_with_fetch(db, oid, fetch)
    }
}

fn prefetch_content_merge_blobs(
    db: &FileObjectDatabase,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    fetch: Option<&dyn PromisorObjectFetch>,
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
            .is_some_and(|(mode, _)| dm::is_mergeable_file_mode(*mode))
            && theirs_entry.is_some_and(|(mode, _)| dm::is_mergeable_file_mode(*mode))
            && base_entry
                .map(|(mode, _)| dm::is_mergeable_file_mode(*mode))
                .unwrap_or(true);
        if !content_mergeable {
            continue;
        }
        for (_, oid) in [base_entry, ours_entry, theirs_entry].into_iter().flatten() {
            let _ = merge_read_blob_with_fetch(db, oid, fetch)?;
        }
    }
    Ok(())
}

// ===== commit/ref helpers =====

pub fn commit_tree_oid(
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

pub fn head_commit_oid(refs: &sley_refs::FileRefStore) -> Result<Option<ObjectId>> {
    use sley_refs::RefTarget;
    match refs.read_ref("HEAD")? {
        Some(RefTarget::Symbolic(branch)) => match refs.read_ref(&branch)? {
            Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
            _ => Ok(None),
        },
        Some(RefTarget::Direct(oid)) => Ok(Some(oid)),
        None => Ok(None),
    }
}

// ===== index entry construction =====

pub fn merge_index_entry(path: &[u8], mode: u32, oid: ObjectId, stage: u16) -> IndexEntry {
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

// ===== worktree materialization =====

pub fn merge_write_worktree_file(
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
        std::fs::create_dir_all(parent)?;
    }
    if is_gitlink(mode) {
        // Gitlink (submodule) entry: the `oid` is a *commit*, not a blob, so it
        // must NOT be written as file content. git's entry.c `write_entry`
        // S_IFGITLINK arm only `mkdir`s the submodule directory
        // (`submodule_move_head` — the embedded checkout — is a higher layer
        // sley does not perform), preserving an already-populated submodule
        // checkout.
        if full.is_dir() {
            return Ok(());
        }
        merge_unlink_path_in_the_way(&full)?;
        std::fs::create_dir_all(&full)?;
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
        std::fs::write(&full, content)?;
    } else {
        std::fs::write(&full, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms =
                std::fs::Permissions::from_mode(if mode == 0o100755 { 0o755 } else { 0o644 });
            std::fs::set_permissions(&full, perms)?;
        }
    }
    Ok(())
}

/// Remove whatever currently occupies `full` (lstat-based, so a dangling symlink
/// is removed as the link, not followed) before a merge materializes a new object
/// there. A directory in the way is removed recursively (D/F transition).
fn merge_unlink_path_in_the_way(full: &Path) -> Result<()> {
    match std::fs::symlink_metadata(full) {
        Ok(metadata) => {
            if metadata.is_dir() {
                if merge_path_is_original_cwd(full) {
                    return merge_refuse_remove_current_working_directory(full);
                }
                match std::fs::remove_dir_all(full) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            } else {
                std::fs::remove_file(full)?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
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
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if !meta.is_dir() => std::fs::remove_file(&candidate)?,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub fn merge_remove_worktree_file(worktree_root: &Path, path: &[u8]) -> Result<()> {
    let rel = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidFormat("non-utf8 worktree path".into()))?;
    let full = worktree_root.join(rel);
    // lstat (symlink_metadata): `Path::exists` follows symlinks and misses a
    // dangling one, leaving it behind on removal.
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_dir() => {
            if merge_path_is_original_cwd(&full) {
                return Ok(());
            }
            // A directory occupies a tracked path being removed: this is a
            // gitlink (submodule checkout). git's entry.c `unlink_entry` ⇒
            // `remove_or_warn(mode, ..)` dispatches on `S_ISGITLINK(mode)` to
            // `rmdir_or_warn` (vs `unlink_or_warn` for blobs/symlinks), so the
            // submodule's *directory* is removed, never `unlink`ed. git first
            // deinits via `submodule_move_head` (a higher layer sley does not
            // perform), then `rmdir`s; `rmdir` of a still-populated submodule
            // fails with ENOTEMPTY and git only *warns*, leaving the directory
            // in place rather than erroring (`warn_if_unremovable`). Mirror that:
            // try to remove the (now-empty-or-not) directory, but never fail the
            // operation on a non-empty submodule directory.
            match std::fs::remove_dir(&full) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // ENOTEMPTY (populated submodule) and friends: git warns and
                    // continues. Match the warn-and-continue, do not propagate.
                    eprintln!("warning: unable to rmdir '{rel}': Directory not empty");
                }
            }
        }
        Ok(_) => std::fs::remove_file(&full)?,
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

pub fn merge_refuse_if_current_working_directory_becomes_file(
    worktree_root: &Path,
    target_entries: &MergeTreeMap,
) -> Result<()> {
    let Some(cwd) = merge_original_cwd_relative_to(worktree_root) else {
        return Ok(());
    };
    if target_entries.iter().any(|(path, (mode, _))| {
        path == &cwd && !is_gitlink(*mode) && (mode & 0o170000) != 0o040000
    }) {
        let full = worktree_root.join(path_from_git_bytes_lossy(&cwd));
        if std::fs::symlink_metadata(&full).is_ok_and(|metadata| metadata.is_dir()) {
            return merge_refuse_remove_current_working_directory(&full);
        }
    }
    Ok(())
}

fn merge_original_cwd_absolute() -> Option<PathBuf> {
    let cwd = sley_core::original_cwd().or_else(|| std::env::current_dir().ok())?;
    Some(std::fs::canonicalize(&cwd).unwrap_or(cwd))
}

fn merge_original_cwd_relative_to(worktree_root: &Path) -> Option<Vec<u8>> {
    let root = std::fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
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
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
        if std::fs::remove_dir(path).is_err() {
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

// ===== three-way tree merge adapter =====

/// Flattened tree map: full git path bytes → (mode, oid).
pub type MergeTreeMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;

/// Per-path outcome of a 3-way tree merge.
// Conflict data is intentionally inline so the merge hot path avoids one heap
// allocation per conflicted path.
#[allow(clippy::large_enum_variant)]
pub enum MergePathResult {
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
        kind: Option<dm::MergeConflictKind>,
        /// True when a textual 3-way content merge ran for this path; drives the
        /// `Auto-merging <path>` info line (git emits it only for content merges).
        auto_merged: bool,
    },
}

pub type MergePathResults = BTreeMap<Vec<u8>, MergePathResult>;
pub type MergeConflictPaths = Vec<Vec<u8>>;
pub type MergeInfoMessages = Vec<dm::MergeInfoMessage>;
pub type MergePathFavorResolver<'a> = dyn Fn(&[u8]) -> dm::MergeFavor + 'a;
pub type MergePathMarkerSizeResolver<'a> = dyn Fn(&[u8]) -> usize + 'a;
pub type MergePathBinaryResolver<'a> = dyn Fn(&[u8]) -> bool + 'a;

/// Complete engine outcome for a three-way merge. Most porcelain adapters only
/// need the reshaped path results; `git merge` additionally persists `tree` as
/// the `AUTO_MERGE` pseudo-ref when conflicts remain.
pub struct ThreeWayMergeOutcome {
    pub results: MergePathResults,
    pub conflicts: MergeConflictPaths,
    pub info_messages: MergeInfoMessages,
    pub tree: ObjectId,
}

/// Rename-detection settings threaded into a 3-way merge. `git merge-recursive`
/// (and `git merge -s recursive/ort`) lets the caller tune these via
/// `--find-renames`/`--rename-threshold`/`--no-renames` and the
/// `merge.renames`/`diff.renames` config; the porcelains that don't expose those
/// knobs use git's defaults. Partial-clone hydration is not part of this struct:
/// it is threaded separately as [`PromisorObjectFetch`].
#[derive(Clone, Copy)]
pub struct RenameMergeConfig {
    pub detect_renames: bool,
    pub rename_threshold: u8,
    pub rename_limit: usize,
    pub directory_renames: dm::DirectoryRenames,
}

/// Styled three-way merge for sequencer operations, preserving the replayed
/// command's `-X` strategy options instead of dropping them at the seam.
#[allow(clippy::too_many_arguments)]
pub fn three_way_merge_trees_styled_with_strategy_options(
    db: &FileObjectDatabase,
    config: &GitConfig,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    style: dm::ConflictStyle,
    strategy_options: &[String],
    fetch: Option<&dyn PromisorObjectFetch>,
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
            rename_threshold: dm::DEFAULT_RENAME_THRESHOLD,
            rename_limit: merge_rename_limit_from_config(config),
            directory_renames: directory_renames_from_config(config),
        },
        fetch,
    )?;
    Ok((results, conflicts))
}

pub fn merge_favor_from_strategy_opt(value: &str) -> Option<dm::MergeFavor> {
    match value {
        "ours" => Some(dm::MergeFavor::Ours),
        "theirs" => Some(dm::MergeFavor::Theirs),
        _ => None,
    }
}

pub fn merge_favor_from_strategy_opts(opts: &[String]) -> dm::MergeFavor {
    let mut favor = dm::MergeFavor::None;
    for opt in opts {
        if let Some(next) = merge_favor_from_strategy_opt(opt) {
            favor = next;
        }
    }
    favor
}

pub fn merge_ws_ignore_from_strategy_opts(opts: &[String]) -> dm::WsIgnore {
    let mut whitespace = dm::WsIgnore::EMPTY;
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

/// `merge.directoryRenames`, mapping to the library's
/// [`dm::DirectoryRenames`]. git's default (when unset or unrecognised) is
/// `conflict`: directory renames are detected but each re-homed path is flagged
/// rather than applied silently.
pub fn directory_renames_from_config(config: &GitConfig) -> dm::DirectoryRenames {
    let value = config
        .get("merge", None, "directoryRenames")
        .map(str::to_string);
    match value.as_deref() {
        Some("false") => dm::DirectoryRenames::False,
        Some("true") => dm::DirectoryRenames::True,
        Some("conflict") | None => dm::DirectoryRenames::Conflict,
        // Unknown values fall back to git's default.
        Some(_) => dm::DirectoryRenames::Conflict,
    }
}

/// The effective inexact-rename matrix cap, mirroring merge-ort's
/// `merge_recursive_config`: `diff.renameLimit` seeds it and
/// `merge.renameLimit` overrides; 0 means "no cap".
pub fn merge_rename_limit_from_config(config: &GitConfig) -> usize {
    // `merge.renameLimit` wins over `diff.renameLimit`; check it first.
    let limit = config
        .get("merge", None, "renameLimit")
        .or_else(|| config.get("diff", None, "renameLimit"))
        .and_then(|value| value.trim().parse::<i64>().ok());
    match limit {
        None => 1000,
        Some(value) if value <= 0 => 0,
        Some(value) => value as usize,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn three_way_merge_trees_inner_with_info_opts(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: dm::MergeFavor,
    style: dm::ConflictStyle,
    ws_ignore: dm::WsIgnore,
    renames: RenameMergeConfig,
    fetch: Option<&dyn PromisorObjectFetch>,
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
        fetch,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn three_way_merge_trees_inner_with_info_opts_and_path_favor(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: dm::MergeFavor,
    style: dm::ConflictStyle,
    ws_ignore: dm::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
    fetch: Option<&dyn PromisorObjectFetch>,
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
        fetch,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn three_way_merge_trees_inner_with_info_opts_and_path_resolvers(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: dm::MergeFavor,
    style: dm::ConflictStyle,
    ws_ignore: dm::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
    path_marker_size: Option<&MergePathMarkerSizeResolver<'_>>,
    fetch: Option<&dyn PromisorObjectFetch>,
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
        fetch,
    )?;
    Ok((outcome.results, outcome.conflicts, outcome.info_messages))
}

#[allow(clippy::too_many_arguments)]
pub fn three_way_merge_trees_outcome_with_info_opts_and_path_resolvers(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    base: &MergeTreeMap,
    ours: &MergeTreeMap,
    theirs: &MergeTreeMap,
    ours_label: &str,
    theirs_label: &str,
    ancestor_label: &str,
    favor: dm::MergeFavor,
    style: dm::ConflictStyle,
    ws_ignore: dm::WsIgnore,
    renames: RenameMergeConfig,
    path_favor: Option<&MergePathFavorResolver<'_>>,
    path_marker_size: Option<&MergePathMarkerSizeResolver<'_>>,
    path_is_binary: Option<&MergePathBinaryResolver<'_>>,
    fetch: Option<&dyn PromisorObjectFetch>,
) -> Result<ThreeWayMergeOutcome> {
    // The shared merge engine only sees an object database, while the host knows
    // how to hydrate promised blobs. Fetch just the blobs a non-trivial textual
    // merge will inspect, preserving partial-clone laziness for unrelated blobs.
    prefetch_content_merge_blobs(db, base, ours, theirs, fetch)?;

    let merge = dm::merge_entry_maps(
        db,
        format,
        base,
        ours,
        theirs,
        &dm::MergeTreesOptions {
            ours_label,
            theirs_label,
            ancestor_label,
            favor,
            path_favor,
            path_marker_size,
            path_is_binary,
            detect_renames: renames.detect_renames,
            rename_threshold: renames.rename_threshold,
            rename_limit: renames.rename_limit,
            // Directory-rename detection only fires when file-rename detection is
            // enabled (it is inferred from the file renames found). With renames
            // off, force it off too so `--no-renames` disables both.
            directory_renames: if renames.detect_renames {
                renames.directory_renames
            } else {
                dm::DirectoryRenames::False
            },
            style,
            ws_ignore,
        },
    )?;

    let tree = merge.tree;
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
            Some(dm::MergeConflictKind::DirRenameLocation {
                back_to_self: false,
                ..
            }) | Some(dm::MergeConflictKind::DirRenameImplicitCollision { .. })
        );
        if entry.conflict.is_some() {
            conflicts.push(entry.path.clone());
            if advisory_location {
                let worktree = match entry.result {
                    Some((mode, oid)) => Some((mode, merge_read_blob_with_fetch(db, &oid, fetch)?)),
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
    Ok(ThreeWayMergeOutcome {
        results,
        conflicts,
        info_messages,
        tree,
    })
}
