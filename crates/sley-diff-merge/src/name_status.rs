//! Tree/index/worktree name-status diff and rename detection.

use sley_core::{
    object_id_for_bytes, BString, GitError, ObjectFormat, ObjectId, RepoPath, Result,
};
use sley_index::{BorrowedIndex, Index, IndexStatCache};
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntries, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::line_diff::DiffAlgorithm;

// ===========================================================================
// Gitlink (submodule) resolution helpers.
//
// A gitlink is a mode-160000 tree/index entry whose oid names the commit an
// embedded repository has checked out. These helpers resolve, for a directory
// in the working tree, (a) the embedded repository's git directory — either a
// `.git` directory or a `.git` *file* carrying a `gitdir: <path>` pointer (the
// layout `git submodule add`/`update` creates, pointing into the
// superproject's `.git/modules/<name>`) — and (b) the commit its HEAD names.
// They are the native equivalent of upstream's `resolve_gitlink_ref()`.
// ===========================================================================

/// Resolve the git directory of an embedded repository whose working tree is
/// at `sub_root`. A `.git` directory is returned as-is; a `.git` file is
/// followed through its `gitdir: <path>` pointer (a relative pointer resolves
/// against `sub_root`). Returns `None` when there is no `.git` entry or the
/// pointer does not name an existing directory.
pub fn gitlink_git_dir(sub_root: &Path) -> Option<PathBuf> {
    let dot_git = sub_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    if !metadata.is_file() {
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = PathBuf::from(target);
    let git_dir = if target.is_absolute() {
        target
    } else {
        sub_root.join(target)
    };
    if git_dir.is_dir() {
        Some(git_dir)
    } else {
        None
    }
}

/// When `sub_root` holds a *broken* gitlink — a `.git` file whose `gitdir:`
/// pointer names a directory that no longer exists (e.g. the submodule's git
/// directory was moved out of `.git/modules/`) — return that unresolved gitdir
/// path. git's status / diff-index fail fatally ("not a git repository: …")
/// here. Returns `None` for a valid gitlink (a `.git` directory, or a `.git`
/// file with a live gitdir) and for an *unpopulated* gitlink (no `.git` entry at
/// all), both of which git treats as non-fatal (the latter as unchanged).
pub fn gitlink_broken_gitdir(sub_root: &Path) -> Option<PathBuf> {
    let dot_git = sub_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if !metadata.is_file() {
        // No `.git` (unpopulated) or a real `.git` directory — not broken.
        return None;
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let target = contents.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target_path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        sub_root.join(target)
    };
    if target_path.is_dir() {
        None
    } else {
        Some(target_path)
    }
}

/// Resolve the commit checked out in the embedded repository at `sub_root`
/// (the value a gitlink entry for that path records): its git directory's
/// HEAD, followed through symbolic refs. `None` when `sub_root` is not a
/// repository or its HEAD does not resolve to a commit (e.g. an unborn
/// branch) — upstream's `resolve_gitlink_ref() < 0` case.
pub fn gitlink_head_oid(sub_root: &Path, format: ObjectFormat) -> Option<ObjectId> {
    let git_dir = gitlink_git_dir(sub_root)?;
    let store = FileRefStore::new(&git_dir, format);
    let mut target = store.read_ref("HEAD").ok()??;
    // Follow symbolic-ref chains defensively (git caps the depth too).
    for _ in 0..10 {
        match target {
            RefTarget::Direct(oid) => return Some(oid),
            RefTarget::Symbolic(name) => target = store.read_ref(&name).ok()??,
        }
    }
    None
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Add { path: RepoPath },
    Delete { path: RepoPath },
    Modify { path: RepoPath },
    Rename { old: RepoPath, new: RepoPath },
    Copy { source: RepoPath, dest: RepoPath },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: RepoPath,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStatus {
    Added,
    Deleted,
    Modified,
    /// A path whose file type (`S_IFMT` bits of the mode) changed between the two
    /// sides — regular↔symlink, regular↔gitlink, symlink↔gitlink. git renders this
    /// as `T` (`DIFF_STATUS_TYPE_CHANGED`, set by `diffcore.h`'s
    /// `DIFF_PAIR_TYPE_CHANGED` before rename/modify resolution). An exec-bit-only
    /// change (100644↔100755) is NOT a typechange — same `S_IFMT`.
    TypeChanged,
    Renamed(u8),
    Copied(u8),
    /// An unmerged (conflicted) path: the index holds higher-stage entries.
    /// git emits a standalone `U <path>` pair (`diff_unmerge`) for it in
    /// addition to the regular worktree-vs-stage-2 modify.
    Unmerged,
}

impl NameStatus {
    pub const fn code(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::TypeChanged => 'T',
            Self::Renamed(_) => 'R',
            Self::Copied(_) => 'C',
            Self::Unmerged => 'U',
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Renamed(score) => format!("R{score:03}"),
            Self::Copied(score) => format!("C{score:03}"),
            _ => self.code().to_string(),
        }
    }
}

/// The bit mask isolating the file-type bits of a git mode (`S_IFMT`). Regular
/// files are `0o100000`, symlinks `0o120000`, gitlinks `0o160000`, trees
/// `0o040000`.
pub const S_IFMT: u32 = 0o170000;

/// Whether a pair of (non-zero) modes constitutes a git "typechange": the file
/// type bits (`S_IFMT`) differ. Mirrors `diffcore.h`'s `DIFF_PAIR_TYPE_CHANGED`
/// (`(S_IFMT & one->mode) != (S_IFMT & two->mode)`). An exec-bit-only change
/// (`0o100644` ↔ `0o100755`) is NOT a typechange — same `S_IFMT`.
#[must_use]
pub const fn is_type_change(old_mode: u32, new_mode: u32) -> bool {
    (old_mode & S_IFMT) != (new_mode & S_IFMT)
}

/// Classify a both-sides-present change whose entries already differ: a
/// [`NameStatus::TypeChanged`] when the modes' `S_IFMT` bits differ, otherwise a
/// plain [`NameStatus::Modified`]. git sets `DIFF_STATUS_TYPE_CHANGED` before any
/// rename/modify resolution (`diff.c` ~6650).
#[must_use]
pub const fn modify_or_type_change(old_mode: u32, new_mode: u32) -> NameStatus {
    if is_type_change(old_mode, new_mode) {
        NameStatus::TypeChanged
    } else {
        NameStatus::Modified
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: NameStatus,
    pub path: BString,
    pub old_path: Option<BString>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub old_oid: Option<ObjectId>,
    pub new_oid: Option<ObjectId>,
}

impl NameStatusEntry {
    pub fn line(&self) -> String {
        if let Some(old_path) = &self.old_path {
            format!(
                "{}\t{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(old_path.as_bytes()),
                String::from_utf8_lossy(self.path.as_bytes())
            )
        } else {
            format!(
                "{}\t{}",
                self.status.label(),
                String::from_utf8_lossy(self.path.as_bytes())
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexGitlinkEntry {
    pub path: BString,
    pub oid: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWorktreeDiff {
    pub entries: Vec<NameStatusEntry>,
    pub staged_gitlinks: Vec<IndexGitlinkEntry>,
}

#[derive(Clone, Copy)]
pub struct IndexWorktreeValidationEntry<'a> {
    pub path: &'a [u8],
    pub mode: u32,
    pub oid: ObjectId,
    pub size: u32,
}

pub struct IndexWorktreeValidatedEntry {
    pub mode: u32,
    pub oid: ObjectId,
}

pub type IndexWorktreeStatCleanValidator<'a> = dyn FnMut(
        IndexWorktreeValidationEntry<'_>,
        &Path,
        &fs::Metadata,
    ) -> Result<Option<IndexWorktreeValidatedEntry>>
    + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffNameStatusOptions {
    pub detect_renames: bool,
    pub detect_copies: bool,
    pub find_copies_harder: bool,
    pub rename_empty: bool,
    pub detect_inexact: bool,
    pub rename_threshold: u8,
    pub copy_threshold: u8,
    pub rename_limit: usize,
}

impl Default for DiffNameStatusOptions {
    fn default() -> Self {
        Self {
            detect_renames: true,
            detect_copies: false,
            find_copies_harder: false,
            rename_empty: true,
            detect_inexact: false,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            copy_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: 0,
        }
    }
}

impl DiffNameStatusOptions {
    pub fn inexact(mut self) -> Self {
        self.detect_inexact = true;
        self
    }
}

/// git's default minimum similarity (as a percentage) for a pair of files to be
/// reported as a rename or copy. Matches `git`'s built-in `-M`/`-C` threshold
/// of 50% (`DEFAULT_RENAME_SCORE` is `MAX_SCORE / 2`).
pub const DEFAULT_RENAME_THRESHOLD: u8 = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenameLimitDiagnostics {
    pub inexact_renames_skipped: bool,
    pub inexact_copies_skipped: bool,
    pub inexact_copies_degraded: bool,
}

impl RenameLimitDiagnostics {
    pub fn any_skipped(self) -> bool {
        self.inexact_renames_skipped || self.inexact_copies_skipped
    }

    pub fn any_warning(self) -> bool {
        self.any_skipped() || self.inexact_copies_degraded
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusWithRenameDiagnostics {
    pub entries: Vec<NameStatusEntry>,
    pub rename_limit: RenameLimitDiagnostics,
}

pub fn diff_name_status_head_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_head_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
        missing_skip_worktree_entries,
        present_skip_worktree_entries,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(head.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
        Some(&missing_skip_worktree_entries),
        Some(&present_skip_worktree_entries),
    )?;
    let changes = if options.detect_inexact {
        let cache = worktree_blob_cache_for_path_set(
            worktree_root,
            &head,
            &worktree,
            &candidate_paths,
            Some(&present_skip_worktree_entries),
            options,
        )?;
        diff_name_status_maps_with_renames_for_path_set(
            &head,
            &worktree,
            &candidate_paths,
            options,
            |oid| cache_or_odb_blob(&cache, &db, oid),
        )?
    } else {
        diff_name_status_maps_for_path_set(&head, &worktree, &candidate_paths, options)?
    };
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

pub fn diff_name_status_head_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_head_index_with_options(git_dir, format, DiffNameStatusOptions::default())
}

pub fn diff_name_status_head_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if options.detect_inexact {
        return Ok(
            diff_name_status_head_index_with_options_and_diagnostics(git_dir, format, options)?
                .entries,
        );
    }
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps(&head, &index, head.keys().chain(index.keys()), options)
}

pub fn diff_name_status_head_index_with_options_and_diagnostics(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<NameStatusWithRenameDiagnostics> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head = head_tree_entries(git_dir, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps_with_renames_and_diagnostics(
        &head,
        &index,
        head.keys().chain(index.keys()),
        options,
        |oid| read_blob_bytes(&db, oid),
    )
}

/// Read an arbitrary tree object's flattened blob entries (recursively) keyed by
/// repository-relative path. This is the tree-side counterpart used by
/// `git diff-index <tree-ish>`: unlike [`head_tree_entries`] it does not consult
/// `HEAD`, so any commit/tag (peeled to a tree) or tree oid can be compared.
///
/// The canonical empty tree (`git hash-object -t tree /dev/null`) is treated as
/// always present and yields no entries, even when the object was never written
/// to the database. git makes the same guarantee, which keeps the common idiom
/// `git diff-index --cached <empty-tree-sha>` working in a fresh repository.
fn tree_entries(
    tree_oid: &ObjectId,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    if *tree_oid == empty_tree_oid(format)? {
        return Ok(entries);
    }
    collect_tree_entries(db, format, tree_oid, Vec::new(), &mut entries)?;
    Ok(entries)
}

/// The well-known oid of the empty tree for `format` (the hash of a zero-length
/// tree object). git hard-codes this value and treats it as always existing.
pub(crate) fn empty_tree_oid(format: ObjectFormat) -> Result<ObjectId> {
    object_id_for_bytes(format, "tree", b"")
}

/// Name-status diff of an arbitrary tree against the index, the engine behind
/// `git diff-index --cached <tree-ish>`. Exact rename/copy detection follows
/// `options`; all blob content comes from the object database.
pub fn diff_name_status_tree_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if options.detect_inexact {
        return Ok(
            diff_name_status_tree_index_with_options_and_diagnostics(
                git_dir, format, tree_oid, options,
            )?
            .entries,
        );
    }
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps(&tree, &index, tree.keys().chain(index.keys()), options)
}

pub fn diff_name_status_tree_index_with_options_and_diagnostics(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<NameStatusWithRenameDiagnostics> {
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let index = read_index_entries(git_dir, format)?;
    diff_name_status_maps_with_renames_and_diagnostics(
        &tree,
        &index,
        tree.keys().chain(index.keys()),
        options,
        |oid| read_blob_bytes(&db, oid),
    )
}

/// Name-status diff of an arbitrary tree against the working tree, the engine
/// behind plain `git diff-index <tree-ish>` (no `--cached`). New-side oids for
/// paths whose worktree contents differ from the index are cleared (rendered as
/// zeros), matching git, which only reports the worktree blob oid when it is
/// known-clean against the index.
pub fn diff_name_status_tree_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let tree = tree_entries(tree_oid, format, &db)?;
    let IndexSnapshot {
        entries: index,
        stat_cache,
        missing_skip_worktree_entries,
        present_skip_worktree_entries,
    } = read_index_snapshot(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let candidate_paths = candidate_path_set(tree.keys().chain(index.keys()));
    let worktree = worktree_entries_for_path_set(
        worktree_root,
        format,
        &candidate_paths,
        &index_gitlinks,
        Some(&stat_cache),
        Some(&missing_skip_worktree_entries),
        Some(&present_skip_worktree_entries),
    )?;
    let changes = if options.detect_inexact {
        let cache = worktree_blob_cache_for_path_set(
            worktree_root,
            &tree,
            &worktree,
            &candidate_paths,
            Some(&present_skip_worktree_entries),
            options,
        )?;
        diff_name_status_maps_with_renames_for_path_set(
            &tree,
            &worktree,
            &candidate_paths,
            options,
            |oid| cache_or_odb_blob(&cache, &db, oid),
        )?
    } else {
        diff_name_status_maps_for_path_set(&tree, &worktree, &candidate_paths, options)?
    };
    Ok(mark_unstaged_worktree_oids_unresolved(
        changes, &index, &worktree,
    ))
}

pub fn diff_name_status_index_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_index_worktree_with_options(
        worktree_root,
        git_dir,
        format,
        DiffNameStatusOptions::default(),
    )
}

pub fn diff_name_status_index_worktree_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    Ok(diff_name_status_index_worktree_with_options_and_gitlinks(
        worktree_root,
        git_dir,
        format,
        options,
    )?
    .entries)
}

pub fn diff_name_status_index_worktree_with_options_and_gitlinks(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<IndexWorktreeDiff> {
    let IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    } = diff_name_status_index_worktree_changes(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        None,
    )?;
    let entries = apply_name_status_options_to_index_worktree_changes(entries, options)?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

pub fn diff_name_status_index_worktree_with_options_and_gitlinks_validated(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
    stat_clean_validator: &mut IndexWorktreeStatCleanValidator<'_>,
) -> Result<IndexWorktreeDiff> {
    let IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    } = diff_name_status_index_worktree_changes(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        Some(stat_clean_validator),
    )?;
    let entries = apply_name_status_options_to_index_worktree_changes(entries, options)?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

fn diff_name_status_index_worktree_changes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut stat_clean_validator: Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<IndexWorktreeDiff> {
    let index_path = sley_index::repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexWorktreeDiff {
                entries: Vec::new(),
                staged_gitlinks: Vec::new(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let index_bytes = fs::read(&index_path)?;
    if let Ok(index) = BorrowedIndex::parse(&index_bytes, format)
        && index.extension(&sley_index::INDEX_EXT_LINK)?.is_none()
        && !index.entries.iter().any(borrowed_entry_is_sparse_dir)
    {
        let (has_non_normal_stage, staged_gitlinks) =
            index_worktree_metadata_for_entries(&index.entries);
        if has_non_normal_stage {
            return diff_name_status_index_worktree_changes_from_snapshot(
                worktree_root,
                git_dir,
                format,
            );
        }
        let stat_cache =
            IndexStatCache::from_index_mtime_only(sley_index::file_mtime_parts(&index_metadata));
        let entries = diff_name_status_index_worktree_changes_for_borrowed_entries(
            worktree_root,
            format,
            &index.entries,
            &stat_cache,
            stat_clean_validator.as_deref_mut(),
        )?;
        return Ok(IndexWorktreeDiff {
            entries,
            staged_gitlinks,
        });
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    let (has_non_normal_stage, staged_gitlinks) =
        index_worktree_metadata_for_entries(&index.entries);
    if has_non_normal_stage {
        return diff_name_status_index_worktree_changes_from_snapshot(
            worktree_root,
            git_dir,
            format,
        );
    }
    let stat_cache =
        IndexStatCache::from_index_mtime_only(sley_index::file_mtime_parts(&index_metadata));
    let entries = diff_name_status_index_worktree_changes_for_entries(
        worktree_root,
        format,
        &index.entries,
        &stat_cache,
        stat_clean_validator.as_deref_mut(),
    )?;
    Ok(IndexWorktreeDiff {
        entries,
        staged_gitlinks,
    })
}

fn borrowed_entry_is_sparse_dir(entry: &sley_index::IndexEntryRef<'_>) -> bool {
    entry.mode == sley_index::SPARSE_DIR_MODE && entry.is_skip_worktree()
}

fn expand_sparse_index_for_worktree_diff(
    mut index: Index,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Index> {
    if !index
        .entries
        .iter()
        .any(sley_index::IndexEntry::is_sparse_dir)
    {
        return Ok(index);
    }

    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut expanded = Vec::with_capacity(index.entries.len());
    for entry in std::mem::take(&mut index.entries) {
        if !entry.is_sparse_dir() {
            expanded.push(entry);
            continue;
        }

        let dir_prefix = entry.path.as_bytes();
        for (rel_path, (mode, oid)) in flatten_tree(&db, format, &entry.oid)? {
            let mut path = dir_prefix.to_vec();
            path.extend_from_slice(&rel_path);
            let mut expanded_entry = sley_index::IndexEntry {
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
                flags: 0,
                flags_extended: 0,
                path: BString::from(path),
            };
            expanded_entry.set_skip_worktree(true);
            expanded_entry.refresh_name_length();
            expanded.push(expanded_entry);
        }
    }

    expanded.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    index.entries = expanded;
    index.clear_sparse_extension()?;
    Ok(index)
}

fn diff_name_status_index_worktree_changes_for_borrowed_entries(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntryRef<'_>],
    stat_cache: &IndexStatCache,
    stat_clean_validator: Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<Vec<NameStatusEntry>> {
    const PARALLEL_SCAN_MIN_ENTRIES: usize = 2048;
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8);
    if stat_clean_validator.is_some() || workers <= 1 || entries.len() < PARALLEL_SCAN_MIN_ENTRIES {
        return diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
            worktree_root,
            format,
            entries,
            stat_cache,
            stat_clean_validator,
        );
    }
    let chunk_size = entries.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
                    worktree_root,
                    format,
                    chunk,
                    stat_cache,
                    None,
                )
            }));
        }
        let mut changes = Vec::new();
        for handle in handles {
            let chunk_changes = handle
                .join()
                .map_err(|_| GitError::Command("diff worker panicked".into()))??;
            changes.extend(chunk_changes);
        }
        Ok(changes)
    })
}

fn diff_name_status_index_worktree_changes_for_entries(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntry],
    stat_cache: &IndexStatCache,
    stat_clean_validator: Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<Vec<NameStatusEntry>> {
    const PARALLEL_SCAN_MIN_ENTRIES: usize = 2048;
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(8);
    if stat_clean_validator.is_some() || workers <= 1 || entries.len() < PARALLEL_SCAN_MIN_ENTRIES {
        return diff_name_status_index_worktree_changes_for_entry_chunk(
            worktree_root,
            format,
            entries,
            stat_cache,
            stat_clean_validator,
        );
    }
    let chunk_size = entries.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                diff_name_status_index_worktree_changes_for_entry_chunk(
                    worktree_root,
                    format,
                    chunk,
                    stat_cache,
                    None,
                )
            }));
        }
        let mut changes = Vec::new();
        for handle in handles {
            let chunk_changes = handle
                .join()
                .map_err(|_| GitError::Command("diff worker panicked".into()))??;
            changes.extend(chunk_changes);
        }
        Ok(changes)
    })
}

fn diff_name_status_index_worktree_changes_for_entry_chunk(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntry],
    stat_cache: &IndexStatCache,
    mut stat_clean_validator: Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes = Vec::new();
    let mut path = PathBuf::from(worktree_root);
    for entry in entries {
        worktree_path_for_repo_path_into(&mut path, worktree_root, entry.path.as_bytes());
        if let Some(change) = index_worktree_change_for_entry(
            &path,
            format,
            entry,
            stat_cache,
            &mut stat_clean_validator,
        )? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn diff_name_status_index_worktree_changes_for_borrowed_entry_chunk(
    worktree_root: &Path,
    format: ObjectFormat,
    entries: &[sley_index::IndexEntryRef<'_>],
    stat_cache: &IndexStatCache,
    mut stat_clean_validator: Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes = Vec::new();
    let mut path = PathBuf::from(worktree_root);
    for entry in entries {
        worktree_path_for_repo_path_into(&mut path, worktree_root, entry.path);
        if let Some(change) = index_worktree_change_for_entry(
            &path,
            format,
            entry,
            stat_cache,
            &mut stat_clean_validator,
        )? {
            changes.push(change);
        }
    }
    Ok(changes)
}

fn index_worktree_metadata_for_entries(
    entries: &[impl WorktreeIndexEntry],
) -> (bool, Vec<IndexGitlinkEntry>) {
    let mut needs_snapshot = false;
    let mut staged_gitlinks = Vec::new();
    for entry in entries {
        if entry.stage() != sley_index::Stage::Normal {
            needs_snapshot = true;
        }
        // Intent-to-add entries (`git add -N`) must take the snapshot path, which
        // diffs them as new files rather than loading their empty-blob id.
        if entry.is_intent_to_add() {
            needs_snapshot = true;
        }
        if sley_index::is_gitlink(entry.mode()) {
            staged_gitlinks.push(IndexGitlinkEntry {
                path: BString::from_bytes(entry.git_path()),
                oid: entry.oid(),
            });
        }
    }
    (needs_snapshot, staged_gitlinks)
}

fn diff_name_status_index_worktree_changes_from_snapshot(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<IndexWorktreeDiff> {
    let IndexSnapshot {
        entries: index,
        stat_cache,
        ..
    } = read_index_snapshot(git_dir, format)?;
    // Intent-to-add (`git add -N`) paths are placeholders: git's `run_diff_files`
    // diffs them as a brand-new file (`/dev/null` → worktree), never loading the
    // recorded empty-blob id. `read_index_snapshot` drops the ITA flag, so read
    // the set of ITA stage-0 paths separately and override their verdict below.
    let intent_to_add_paths = read_intent_to_add_paths(git_dir, format)?;
    // `read_index_snapshot` collapses each path to a single entry; for an
    // unmerged path it keeps the last-written stage. To match git's
    // `run_diff_files` we need the conflict stages, so read them separately:
    // git diffs the worktree against the "ours" stage (stage 2, the default
    // `diff_unmerged_stage`) and additionally emits a standalone `U <path>`
    // pair via `diff_unmerge` (diff-lib.c).
    let unmerged = read_unmerged_stages(git_dir, format)?;
    let index_gitlinks = index_gitlinks(&index);
    let staged_gitlinks = index_gitlinks
        .iter()
        .map(|(path, oid)| IndexGitlinkEntry {
            path: BString::from_bytes(path),
            oid: *oid,
        })
        .collect();
    let mut changes = Vec::new();
    for (git_path, left) in &index {
        // For a conflicted path git first queues the `U` pair, then compares the
        // worktree against stage 2 (ours). The snapshot's collapsed `left` may
        // be the wrong stage, so override it with the stage-2 entry when present.
        let conflict_stages = unmerged.get(git_path);
        let right = worktree_entry_for_path(
            worktree_root,
            format,
            git_path,
            &index_gitlinks,
            Some(&stat_cache),
            None,
            None,
        )?;
        if conflict_stages.is_some() {
            // git's `diff_unmerge` makes a pair with a null old side and the
            // worktree mode on the new side (diff-lib.c `wt_mode`); the oids stay
            // zero. The raw line is `:000000 <wt_mode> 0..0 0..0 U <path>`.
            changes.push(NameStatusEntry {
                status: NameStatus::Unmerged,
                path: git_path.clone().into(),
                old_path: None,
                old_mode: None,
                new_mode: right.as_ref().map(|entry| entry.mode),
                old_oid: None,
                new_oid: None,
            });
        }
        // The index side for the modify comparison: stage 2 (ours) for a
        // conflict, otherwise the normal stage-0 entry. If the conflict has no
        // stage-2 (deleted on our side / added by them), git has no entry to
        // diff the worktree against, so it emits only the `U` line.
        let left = match conflict_stages {
            Some(stages) => match stages.ours.as_ref() {
                Some(ours) => ours,
                None => continue,
            },
            None => left,
        };
        // Intent-to-add placeholder: git's `run_diff_files` diffs it as a new
        // file. With the worktree file present, queue an `Added` pair whose old
        // side is null (`/dev/null` → worktree blob); with the file gone, an ITA
        // entry yields no diff-files entry (there is nothing to add).
        if intent_to_add_paths.contains(git_path.as_slice()) {
            if let Some(right) = right {
                changes.push(NameStatusEntry {
                    status: NameStatus::Added,
                    path: git_path.clone().into(),
                    old_path: None,
                    old_mode: None,
                    new_mode: Some(right.mode),
                    old_oid: None,
                    new_oid: Some(right.oid),
                });
            }
            continue;
        }
        let Some(right) = right else {
            changes.push(NameStatusEntry {
                status: NameStatus::Deleted,
                path: git_path.clone().into(),
                old_path: None,
                old_mode: Some(left.mode),
                new_mode: None,
                old_oid: Some(left.oid),
                new_oid: None,
            });
            continue;
        };
        if right != *left {
            changes.push(NameStatusEntry {
                status: modify_or_type_change(left.mode, right.mode),
                path: git_path.clone().into(),
                old_path: None,
                old_mode: Some(left.mode),
                new_mode: Some(right.mode),
                old_oid: Some(left.oid),
                new_oid: Some(right.oid),
            });
        }
    }
    Ok(IndexWorktreeDiff {
        entries: changes,
        staged_gitlinks,
    })
}

/// The conflict stages recorded for one unmerged index path.
struct ConflictStages {
    ours: Option<TrackedEntry>,
}

/// Read the higher-stage (conflict) index entries, keyed by path, recording the
/// "ours" (stage 2) entry git diffs the worktree against. Paths with only a
/// stage-0 entry are absent from the result.
fn read_unmerged_stages(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, ConflictStages>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = sley_index::read_repository_index(git_dir, format)?;
    let mut out: BTreeMap<Vec<u8>, ConflictStages> = BTreeMap::new();
    for entry in &index.entries {
        let stage = entry.stage();
        if stage == sley_index::Stage::Normal {
            continue;
        }
        let path = entry.path.clone().into_bytes();
        let slot = out.entry(path).or_insert(ConflictStages { ours: None });
        if stage == sley_index::Stage::Ours {
            slot.ours = Some(TrackedEntry {
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(out)
}

fn apply_name_status_options_to_index_worktree_changes(
    mut changes: Vec<NameStatusEntry>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if options.detect_renames {
        changes = detect_exact_renames_from_changes(changes, options.rename_empty);
    } else if options.detect_copies {
        changes.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    }
    Ok(changes)
}

fn detect_exact_renames_from_changes(
    changes: Vec<NameStatusEntry>,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let added = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Added)
        .map(|(idx, entry)| (idx, entry.path.clone()))
        .collect::<Vec<_>>();
    let mut sources = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Deleted)
        .filter_map(|(idx, entry)| {
            entry
                .old_oid
                .map(|oid| (idx, entry.path.clone(), oid, entry.old_mode))
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

    let mut src_used = vec![false; sources.len()];
    let mut consumed_added = BTreeSet::new();
    let mut consumed_deleted = BTreeSet::new();
    let mut result = Vec::new();

    for (idx, new_path) in &added {
        let Some(right_oid) = changes[*idx].new_oid else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&right_oid) {
            continue;
        };
        let mut best: Option<usize> = None;
        let mut best_score = -1i32;
        for (source_idx, (_, src_path, src_oid, _)) in sources.iter().enumerate() {
            if src_used[source_idx] || *src_oid != right_oid {
                continue;
            }
            let score = 1 + i32::from(path_basename(src_path) == path_basename(new_path));
            if score > best_score {
                best = Some(source_idx);
                best_score = score;
                if score == 2 {
                    break;
                }
            }
        }
        if let Some(source_idx) = best {
            src_used[source_idx] = true;
            consumed_added.insert(*idx);
            let (old_idx, old_path, old_oid, old_mode) = &sources[source_idx];
            consumed_deleted.insert(*old_idx);
            if old_path.as_bytes() == new_path.as_bytes()
                && *old_mode == changes[*idx].new_mode
                && *old_oid == right_oid
            {
                continue;
            }
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: new_path.clone(),
                old_path: Some(old_path.clone()),
                old_mode: *old_mode,
                new_mode: changes[*idx].new_mode,
                old_oid: Some(*old_oid),
                new_oid: Some(right_oid),
            });
        }
    }

    for (index, entry) in changes.into_iter().enumerate() {
        if consumed_added.contains(&index) || consumed_deleted.contains(&index) {
            continue;
        }
        result.push(entry);
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Index-vs-worktree name-status for **`git diff-files`** (plumbing), which
/// selects changed paths by the cached *stat* rather than by content.
///
/// This is the crucial difference from [`diff_name_status_index_worktree_with_options`]
/// (the engine behind porcelain `git diff`): porcelain `git diff` refreshes the
/// index first, so a stat-dirty-but-content-identical entry (a `touch`ed file, or
/// a freshly `rm --cached`-then-`reset --no-refresh` entry with a zeroed cached
/// stat) is re-stamped clean and suppressed. `git diff-files` does **not** refresh
/// — it reports every entry whose cached stat fails to prove it clean as `M`,
/// without re-hashing the content to "rescue" it (`builtin/diff.c` →
/// `run_diff_files` → `ie_match_stat`). The raw / name-only / name-status output
/// and the `--quiet`/`--exit-code` status therefore list such entries even when
/// the content is byte-identical; patch/stat output, which diffs actual content,
/// renders them as an empty hunk.
///
/// We layer that stat-based selection on top of the content-based diff: the
/// content diff already catches adds/deletes/genuine-content modifies (with
/// rename detection), and we then append a `Modified` entry for any stage-0 path
/// whose worktree file is present and whose cached stat is dirty per
/// [`IndexStatCache::index_entry_worktree_stat_dirty`] but which the content diff
/// did not already report. Content-identical stat-dirty entries cannot be rename
/// sources/targets (their content is unchanged), so they never interact with the
/// rename machinery — they are plain `M`.
pub fn diff_name_status_index_worktree_for_diff_files_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let changes =
        diff_name_status_index_worktree_with_options(worktree_root, git_dir, format, options)?;
    augment_with_stat_dirty_entries(worktree_root, git_dir, format, changes)
}

/// Append a `Modified` entry for every stage-0 index path whose worktree file is
/// present and whose cached stat is dirty (`ce_match_stat` "changed") but which
/// `content_changes` did not already report. The result is re-sorted by path so
/// the merged set keeps git's diff-queue ordering. New-side oids on the added
/// entries are left `None` (rendered as zeros in raw output), matching git, which
/// reports the worktree blob oid only for entries it has hashed.
fn augment_with_stat_dirty_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut content_changes: Vec<NameStatusEntry>,
) -> Result<Vec<NameStatusEntry>> {
    let IndexSnapshot {
        entries: index,
        stat_cache,
        ..
    } = read_index_snapshot(git_dir, format)?;
    // Paths the content diff already accounts for (by new-side path, the position
    // git queues a pair at — a rename's destination, a modify/add/delete's path).
    let already_reported: BTreeSet<&[u8]> = content_changes
        .iter()
        .map(|entry| entry.path.as_bytes())
        .collect();
    let mut extras = Vec::new();
    for (git_path, tracked) in &index {
        if already_reported.contains(git_path.as_slice()) {
            continue;
        }
        let Some(cached) = stat_cache.entry_for_git_path(git_path) else {
            continue;
        };
        // Gitlinks (submodules) have their own dirtiness model and are not stat-
        // compared here; the content diff already handles changed gitlink oids.
        if sley_index::is_gitlink(tracked.mode) {
            continue;
        }
        let path = worktree_path_for_repo_path(worktree_root, git_path);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            // A missing worktree file is a deletion, which the content diff
            // already reports; nothing to add here.
            continue;
        };
        if !(metadata.is_file() || metadata.file_type().is_symlink()) {
            continue;
        }
        match stat_cache.index_entry_worktree_stat_verdict(cached, &metadata) {
            sley_index::StatVerdict::Clean => continue,
            sley_index::StatVerdict::Dirty => {}
            // A racily-clean entry must be resolved by content: git re-hashes it
            // (`ce_compare_data`) and only reports `M` when the worktree bytes
            // actually differ from the cached oid — so a `touch`ed-then-re-`add`ed
            // file (same-second mtime as the index) stays clean.
            sley_index::StatVerdict::RacyNeedsContentCheck => {
                if worktree_oid_matches_index(worktree_root, git_path, &metadata, tracked, format)?
                {
                    continue;
                }
            }
        }
        extras.push(NameStatusEntry {
            status: NameStatus::Modified,
            path: git_path.clone().into(),
            old_path: None,
            old_mode: Some(tracked.mode),
            new_mode: Some(tracked.mode),
            old_oid: Some(tracked.oid),
            new_oid: None,
        });
    }
    if !extras.is_empty() {
        content_changes.extend(extras);
        content_changes
            .sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    }
    Ok(content_changes)
}

/// Hash the worktree file or symlink at `path` into the `(mode, oid)` blob entry
/// git would record for it. `metadata` must already describe a regular file or a
/// symlink — gitlink and directory classification is the caller's concern, since
/// those need the index/HEAD context this leaf does not have. This is the single
/// owner of the symlink-vs-regular body-source and mode split that `diff-files`
/// ([`worktree_oid_matches_index`]), the candidate-path collector
/// ([`worktree_entry_for_path`]), and the index↔worktree walk
/// ([`index_worktree_change_for_entry`]) all share; before consolidation each
/// open-coded it with a bare `0o120000`.
fn classify_worktree_entry(
    path: &Path,
    metadata: &fs::Metadata,
    format: ObjectFormat,
) -> Result<TrackedEntry> {
    let is_symlink = metadata.file_type().is_symlink();
    let body = if is_symlink {
        symlink_target_bytes(path)?
    } else {
        fs::read(path)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    let mode = if is_symlink {
        sley_index::SYMLINK_MODE
    } else {
        file_mode(metadata)
    };
    Ok(TrackedEntry { mode, oid })
}

/// Whether the worktree file at `git_path` hashes to the index entry's oid (mode
/// included). Used to resolve a racily-clean `diff-files` entry: git re-hashes the
/// content and only reports it changed when the bytes truly differ. Shares the
/// worktree-oid computation with [`worktree_entry_for_path`] via
/// [`classify_worktree_entry`].
fn worktree_oid_matches_index(
    worktree_root: &Path,
    git_path: &[u8],
    metadata: &fs::Metadata,
    index_entry: &TrackedEntry,
    format: ObjectFormat,
) -> Result<bool> {
    let path = worktree_path_for_repo_path(worktree_root, git_path);
    let entry = classify_worktree_entry(&path, metadata, format)?;
    Ok(entry.oid == index_entry.oid && entry.mode == index_entry.mode)
}

pub fn diff_name_status_trees_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    if options.detect_inexact {
        return Ok(
            diff_name_status_trees_with_options_and_diagnostics(
                db, format, left_tree, right_tree, options,
            )?
            .entries,
        );
    }
    if !options.detect_copies {
        let mut changes = changed_tree_name_status_entries(db, format, left_tree, right_tree)?;
        if options.detect_renames {
            changes = detect_exact_renames_from_changes(changes, options.rename_empty);
        }
        return Ok(changes);
    }

    // `--find-copies-harder` may pair an *unchanged* left-side file as a copy
    // source, so it needs the complete left map; every other mode only consults
    // changed paths, so the pruned simultaneous walk (which skips identical
    // subtrees) suffices and produces byte-identical output.
    let needs_full_maps = options.detect_copies && options.find_copies_harder;
    let (left_entries, right_entries) = if needs_full_maps {
        collect_full_tree_pair(db, format, left_tree, right_tree)?
    } else {
        changed_tree_entries(db, format, left_tree, right_tree)?
    };
    diff_name_status_maps(
        &left_entries,
        &right_entries,
        left_entries.keys().chain(right_entries.keys()),
        options,
    )
}

pub fn diff_name_status_empty_tree_with_options(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let left_entries = BTreeMap::new();
    let mut right_entries = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right_entries)?;
    if options.detect_inexact {
        return diff_name_status_maps_with_renames(
            &left_entries,
            &right_entries,
            right_entries.keys(),
            options,
            |oid| read_blob_bytes(db, oid),
        );
    }
    diff_name_status_maps(&left_entries, &right_entries, right_entries.keys(), options)
}

pub fn diff_name_status_trees_with_options_and_diagnostics(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    options: DiffNameStatusOptions,
) -> Result<NameStatusWithRenameDiagnostics> {
        if !options.detect_copies {
        let mut diagnostics = RenameLimitDiagnostics::default();
        let mut changes = changed_tree_name_status_entries(db, format, left_tree, right_tree)?;
        if options.detect_renames {
            changes = detect_exact_renames_from_changes(changes, options.rename_empty);
        }
        if options.detect_renames && options.detect_inexact {
            diagnostics.inexact_renames_skipped = inexact_rename_limit_exceeded(&changes, &options);
            changes = detect_inexact_renames(changes, &options, &|oid| read_blob_bytes(db, oid));
        }
        return Ok(NameStatusWithRenameDiagnostics {
            entries: changes,
            rename_limit: diagnostics,
        });
    }

    // See `diff_name_status_trees_with_options`: only `--find-copies-harder`
    // needs unchanged left entries as copy sources; otherwise the pruned walk
    // (skipping identical subtrees) yields identical output far more cheaply.
    let needs_full_maps = options.detect_copies && options.find_copies_harder;
    let (left_entries, right_entries) = if needs_full_maps {
        collect_full_tree_pair(db, format, left_tree, right_tree)?
    } else {
        changed_tree_entries(db, format, left_tree, right_tree)?
    };
    diff_name_status_maps_with_renames_and_diagnostics(
        &left_entries,
        &right_entries,
        left_entries.keys().chain(right_entries.keys()),
        options,
        |oid| read_blob_bytes(db, oid),
    )
}

/// Read a blob's raw bytes from the ODB, returning `None` if the object cannot
/// be read or is not a blob. Used as the similarity-scoring blob fetcher; a
/// missing object simply makes a candidate pair non-similar rather than failing
/// the whole diff.
pub(crate) fn read_blob_bytes(db: &FileObjectDatabase, oid: &ObjectId) -> Option<Arc<[u8]>> {
    match db.read_object(oid) {
        Ok(object) if object.object_type == ObjectType::Blob => {
            Some(Arc::from(object.body.clone().into_boxed_slice()))
        }
        _ => None,
    }
}

/// Build the raw per-path add/delete/modify change list (before any rename or
/// copy detection) from the two entry maps and the candidate path set.
fn raw_name_status_changes_for_unique_paths<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: impl Iterator<Item = &'a Vec<u8>>,
) -> Vec<NameStatusEntry> {
    let mut changes = Vec::new();
    for path in paths {
        let left = left_entries.get(path);
        let right = right_entries.get(path);
        let status = match (left, right) {
            (None, Some(_)) => Some(NameStatus::Added),
            (Some(_), None) => Some(NameStatus::Deleted),
            (Some(left), Some(right)) if left != right => {
                Some(modify_or_type_change(left.mode, right.mode))
            }
            _ => None,
        };
        if let Some(status) = status {
            changes.push(NameStatusEntry {
                status,
                path: path.clone().into(),
                old_path: None,
                old_mode: left.map(|entry| entry.mode),
                new_mode: right.map(|entry| entry.mode),
                old_oid: left.map(|entry| entry.oid),
                new_oid: right.map(|entry| entry.oid),
            });
        }
    }
    changes
}

pub(crate) fn diff_name_status_maps<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let paths = candidate_path_set(candidate_paths);
    diff_name_status_maps_for_path_set(left_entries, right_entries, &paths, options)
}

fn diff_name_status_maps_for_path_set(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    diff_name_status_maps_for_unique_paths(
        left_entries,
        right_entries,
        candidate_paths.iter(),
        options,
    )
}

fn diff_name_status_maps_for_unique_paths<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if options.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, options.rename_empty);
    }
    if options.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            options.find_copies_harder,
            options.rename_empty,
        );
    }
    Ok(changes)
}

/// Like [`diff_name_status_maps`], but additionally runs inexact (similarity)
/// rename/copy detection when `options.detect_inexact` is set.
///
/// `fetch_blob` resolves an [`ObjectId`] to that blob's raw bytes; it is only
/// consulted for the candidate pairs considered during inexact detection, and
/// only when inexact detection is enabled. A pair whose blob bytes cannot be
/// fetched is simply skipped (treated as not similar), so a missing object never
/// fails the whole diff.
pub(crate) fn diff_name_status_maps_with_renames<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Result<Vec<NameStatusEntry>> {
    Ok(diff_name_status_maps_with_renames_and_diagnostics(
        left_entries,
        right_entries,
        candidate_paths,
        options,
        fetch_blob,
    )?
    .entries)
}

fn diff_name_status_maps_with_renames_and_diagnostics<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Result<NameStatusWithRenameDiagnostics> {
    let paths = candidate_path_set(candidate_paths);
    diff_name_status_maps_with_renames_for_path_set_and_diagnostics(
        left_entries,
        right_entries,
        &paths,
        options,
        fetch_blob,
    )
}

fn diff_name_status_maps_with_renames_for_path_set(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: DiffNameStatusOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Result<Vec<NameStatusEntry>> {
    Ok(
        diff_name_status_maps_with_renames_for_path_set_and_diagnostics(
            left_entries,
            right_entries,
            candidate_paths,
            options,
            fetch_blob,
        )?
        .entries,
    )
}

fn diff_name_status_maps_with_renames_for_path_set_and_diagnostics(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    options: DiffNameStatusOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Result<NameStatusWithRenameDiagnostics> {
    diff_name_status_maps_with_renames_for_unique_paths_and_diagnostics(
        left_entries,
        right_entries,
        candidate_paths.iter(),
        options,
        fetch_blob,
    )
}

fn diff_name_status_maps_with_renames_for_unique_paths_and_diagnostics<'a>(
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    options: DiffNameStatusOptions,
    fetch_blob: impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Result<NameStatusWithRenameDiagnostics> {
        let mut diagnostics = RenameLimitDiagnostics::default();
    let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if options.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, options.rename_empty);
    }
    // Inexact rename detection runs after exact renames so exact matches keep
    // priority (and their score of 100). It only fires when rename detection is
    // enabled at all, mirroring git's `-M`.
    if options.detect_renames && options.detect_inexact {
        diagnostics.inexact_renames_skipped = inexact_rename_limit_exceeded(&changes, &options);
        changes = detect_inexact_renames(changes, &options, &fetch_blob);
    }
    if options.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            options.find_copies_harder,
            options.rename_empty,
        );
    }
    if options.detect_copies && options.detect_inexact {
        match inexact_copy_limit_outcome(&changes, left_entries, &options) {
            InexactCopyLimitOutcome::Full => {}
            InexactCopyLimitOutcome::DegradedToChangedSources => {
                diagnostics.inexact_copies_degraded = true;
            }
            InexactCopyLimitOutcome::Skip => {
                diagnostics.inexact_copies_skipped = true;
            }
        }
        changes = detect_inexact_copies(changes, left_entries, &options, &fetch_blob);
    }
    Ok(NameStatusWithRenameDiagnostics {
        entries: changes,
        rename_limit: diagnostics,
    })
}

fn changed_tree_name_status_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<Vec<NameStatusEntry>> {
    let mut changes = Vec::new();
    if left_tree != right_tree {
        diff_tree_pair_entries(db, format, left_tree, right_tree, &[], &mut changes)?;
    }
    sort_name_status_entries_by_path(&mut changes);
    Ok(changes)
}

fn diff_tree_pair_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    prefix: &[u8],
    changes: &mut Vec<NameStatusEntry>,
) -> Result<()> {
    let left_entries = read_tree_object(db, format, left_tree)?.entries;
    let right_entries = read_tree_object(db, format, right_tree)?.entries;
    let mut left_idx = 0usize;
    let mut right_idx = 0usize;

    while left_idx < left_entries.len() || right_idx < right_entries.len() {
        match (left_entries.get(left_idx), right_entries.get(right_idx)) {
            (Some(left_entry), Some(right_entry)) => match left_entry.name.cmp(&right_entry.name) {
                std::cmp::Ordering::Equal => {
                    let left_name = left_entry.name.clone();
                    let right_name = right_entry.name.clone();
                    let left_start = left_idx;
                    let right_start = right_idx;
                    while left_idx < left_entries.len() && left_entries[left_idx].name == left_name
                    {
                        left_idx += 1;
                    }
                    while right_idx < right_entries.len()
                        && right_entries[right_idx].name == right_name
                    {
                        right_idx += 1;
                    }

                    let left_group = &left_entries[left_start..left_idx];
                    let right_group = &right_entries[right_start..right_idx];
                    let paired = left_group.len().min(right_group.len());
                    for idx in 0..paired {
                        diff_tree_entry_pair_entries(
                            db,
                            format,
                            prefix,
                            Some(&left_group[idx]),
                            Some(&right_group[idx]),
                            changes,
                        )?;
                    }
                    for entry in &left_group[paired..] {
                        diff_tree_entry_pair_entries(
                            db,
                            format,
                            prefix,
                            Some(entry),
                            None,
                            changes,
                        )?;
                    }
                    for entry in &right_group[paired..] {
                        diff_tree_entry_pair_entries(
                            db,
                            format,
                            prefix,
                            None,
                            Some(entry),
                            changes,
                        )?;
                    }
                }
                std::cmp::Ordering::Less => {
                    let left_name = left_entry.name.clone();
                    while left_idx < left_entries.len() && left_entries[left_idx].name == left_name
                    {
                        diff_tree_entry_pair_entries(
                            db,
                            format,
                            prefix,
                            Some(&left_entries[left_idx]),
                            None,
                            changes,
                        )?;
                        left_idx += 1;
                    }
                }
                std::cmp::Ordering::Greater => {
                    let right_name = right_entry.name.clone();
                    while right_idx < right_entries.len()
                        && right_entries[right_idx].name == right_name
                    {
                        diff_tree_entry_pair_entries(
                            db,
                            format,
                            prefix,
                            None,
                            Some(&right_entries[right_idx]),
                            changes,
                        )?;
                        right_idx += 1;
                    }
                }
            },
            (Some(left_entry), None) => {
                let left_name = left_entry.name.clone();
                while left_idx < left_entries.len() && left_entries[left_idx].name == left_name {
                    diff_tree_entry_pair_entries(
                        db,
                        format,
                        prefix,
                        Some(&left_entries[left_idx]),
                        None,
                        changes,
                    )?;
                    left_idx += 1;
                }
            }
            (None, Some(right_entry)) => {
                let right_name = right_entry.name.clone();
                while right_idx < right_entries.len() && right_entries[right_idx].name == right_name
                {
                    diff_tree_entry_pair_entries(
                        db,
                        format,
                        prefix,
                        None,
                        Some(&right_entries[right_idx]),
                        changes,
                    )?;
                    right_idx += 1;
                }
            }
            (None, None) => break,
        }
    }

    Ok(())
}

fn diff_tree_entry_pair_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    prefix: &[u8],
    left_entry: Option<&TreeEntry>,
    right_entry: Option<&TreeEntry>,
    changes: &mut Vec<NameStatusEntry>,
) -> Result<()> {
    let left_is_tree = left_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);
    let right_is_tree = right_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);

    if let (Some(left_entry), Some(right_entry)) = (left_entry, right_entry) {
        if left_is_tree && right_is_tree {
            if left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            return diff_tree_pair_entries(
                db,
                format,
                &left_entry.oid,
                &right_entry.oid,
                &path,
                changes,
            );
        }
        if !left_is_tree && !right_is_tree {
            if left_entry.mode == right_entry.mode && left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            changes.push(NameStatusEntry {
                status: modify_or_type_change(left_entry.mode, right_entry.mode),
                path: path.into(),
                old_path: None,
                old_mode: Some(left_entry.mode),
                new_mode: Some(right_entry.mode),
                old_oid: Some(left_entry.oid),
                new_oid: Some(right_entry.oid),
            });
            return Ok(());
        }
    }

    if let Some(left_entry) = left_entry {
        let path = join_tree_path(prefix, left_entry.name.as_bytes());
        collect_tree_side_changes(
            db,
            format,
            path,
            left_entry.mode,
            left_entry.oid,
            NameStatus::Deleted,
            changes,
        )?;
    }
    if let Some(right_entry) = right_entry {
        let path = join_tree_path(prefix, right_entry.name.as_bytes());
        collect_tree_side_changes(
            db,
            format,
            path,
            right_entry.mode,
            right_entry.oid,
            NameStatus::Added,
            changes,
        )?;
    }
    Ok(())
}

fn collect_tree_side_changes(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
    status: NameStatus,
    changes: &mut Vec<NameStatusEntry>,
) -> Result<()> {
    if mode == TREE_ENTRY_MODE {
        for entry in read_tree_object(db, format, &oid)?.entries {
            let child_path = join_tree_path(&path, entry.name.as_bytes());
            collect_tree_side_changes(
                db, format, child_path, entry.mode, entry.oid, status, changes,
            )?;
        }
        return Ok(());
    }

    changes.push(NameStatusEntry {
        status,
        path: path.into(),
        old_path: None,
        old_mode: (status == NameStatus::Deleted).then_some(mode),
        new_mode: (status == NameStatus::Added).then_some(mode),
        old_oid: (status == NameStatus::Deleted).then_some(oid),
        new_oid: (status == NameStatus::Added).then_some(oid),
    });
    Ok(())
}

fn sort_name_status_entries_by_path(changes: &mut [NameStatusEntry]) {
    changes.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
}

fn detect_exact_renames(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let added = changes
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.status == NameStatus::Added)
        .map(|(idx, entry)| (idx, entry.path.clone()))
        .collect::<Vec<_>>();
    // Candidate sources in path order (git's `rename_src` ordering), so the
    // best-source search tie-breaks deterministically.
    let mut sources = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Deleted)
        .filter_map(|entry| {
            left_entries
                .get(entry.path.as_bytes())
                .map(|left| (entry.path.clone(), left.oid))
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let mut src_used = vec![false; sources.len()];
    let mut consumed = BTreeSet::new();
    let mut renamed_old_paths = BTreeSet::new();
    let mut result = Vec::new();

    // git's `find_identical_files`: for each destination, among the still-unused
    // sources with the identical OID, prefer one that shares the destination's
    // basename (score 2 short-circuits; otherwise the first such source wins).
    // Iterating destinations (not sources) is what lets a same-basename source
    // win over an alphabetically-earlier different-basename one.
    for (idx, new_path) in &added {
        let Some(right) = right_entries.get(new_path.as_bytes()) else {
            continue;
        };
        if !rename_empty && is_empty_blob_oid(&right.oid) {
            continue;
        }
        let mut best: Option<usize> = None;
        let mut best_score = -1i32;
        for (si, (src_path, src_oid)) in sources.iter().enumerate() {
            if src_used[si] || *src_oid != right.oid {
                continue;
            }
            let score = 1 + i32::from(path_basename(src_path) == path_basename(new_path));
            if score > best_score {
                best = Some(si);
                best_score = score;
                if score == 2 {
                    break;
                }
            }
        }
        if let Some(si) = best {
            src_used[si] = true;
            consumed.insert(*idx);
            let old_path = sources[si].0.clone();
            let left = &left_entries[old_path.as_bytes()];
            renamed_old_paths.insert(old_path.clone());
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(100),
                path: new_path.clone(),
                old_path: Some(old_path),
                old_mode: Some(left.mode),
                new_mode: Some(right.mode),
                old_oid: Some(left.oid),
                new_oid: Some(right.oid),
            });
        }
    }

    for (idx, entry) in changes.into_iter().enumerate() {
        if entry.status == NameStatus::Added && consumed.contains(&idx) {
            continue;
        }
        if entry.status == NameStatus::Deleted && renamed_old_paths.contains(&entry.path) {
            continue;
        }
        result.push(entry);
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn detect_exact_copies(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    find_copies_harder: bool,
    rename_empty: bool,
) -> Vec<NameStatusEntry> {
    let changed_sources = changes
        .iter()
        .filter(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified))
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let source_paths = left_entries
        .keys()
        .filter(|path| find_copies_harder || changed_sources.contains(path.as_slice()))
        .cloned()
        .collect::<Vec<_>>();

    let mut result = Vec::new();
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(right) = right_entries.get(entry.path.as_bytes()) else {
            result.push(entry);
            continue;
        };
        if let Some(old_path) = source_paths.iter().find(|old_path| {
            old_path.as_slice() != entry.path.as_bytes()
                && left_entries.get(*old_path).is_some_and(|left| {
                    left.oid == right.oid && (rename_empty || !is_empty_blob_oid(&left.oid))
                })
        }) {
            result.push(NameStatusEntry {
                status: NameStatus::Copied(100),
                path: entry.path,
                old_path: Some(old_path.clone().into()),
                old_mode: left_entries
                    .get(old_path.as_slice())
                    .map(|entry| entry.mode),
                new_mode: entry.new_mode,
                old_oid: left_entries.get(old_path.as_slice()).map(|entry| entry.oid),
                new_oid: entry.new_oid,
            });
        } else {
            result.push(entry);
        }
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Old-side metadata of a rename source, snapshotted before the source delete
/// entry is consumed so it can be attached to the renamed destination.
#[derive(Debug, Clone)]
struct RenameSourceMeta {
    path: BString,
    mode: Option<u32>,
    oid: Option<ObjectId>,
}

/// A scored candidate pairing of a deleted source with an added destination,
/// used to order inexact-rename assignment best-match-first.
struct ScoredPair {
    /// Index into the `deleted` candidate list.
    src: usize,
    /// Index into the `added` candidate list.
    dst: usize,
    /// Similarity percentage in `0..=100`.
    score: u8,
}

fn rename_limit_exceeded(source_count: usize, dest_count: usize, rename_limit: usize) -> bool {
    rename_limit > 0
        && source_count
            .saturating_mul(dest_count)
            .gt(&rename_limit.saturating_mul(rename_limit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InexactCopyLimitOutcome {
    Full,
    DegradedToChangedSources,
    Skip,
}

fn inexact_rename_candidates(
    changes: &[NameStatusEntry],
    options: &DiffNameStatusOptions,
) -> (usize, usize) {
    let mut deleted = 0usize;
    let mut added = 0usize;
    for entry in changes {
        match entry.status {
            NameStatus::Deleted => {
                if entry
                    .old_oid
                    .as_ref()
                    .is_some_and(|oid| options.rename_empty || !is_empty_blob_oid(oid))
                {
                    deleted += 1;
                }
            }
            NameStatus::Added => {
                if entry
                    .new_oid
                    .as_ref()
                    .is_some_and(|oid| options.rename_empty || !is_empty_blob_oid(oid))
                {
                    added += 1;
                }
            }
            _ => {}
        }
    }
    (deleted, added)
}

fn inexact_rename_limit_exceeded(
    changes: &[NameStatusEntry],
    options: &DiffNameStatusOptions,
) -> bool {
    let (source_count, dest_count) = inexact_rename_candidates(changes, options);
    rename_limit_exceeded(source_count, dest_count, options.rename_limit)
}

fn changed_copy_sources(changes: &[NameStatusEntry]) -> BTreeSet<BString> {
    changes
        .iter()
        .filter(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified))
        .map(|entry| entry.path.clone())
        .collect()
}

fn inexact_copy_source_count(
    changed_sources: &BTreeSet<BString>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    options: &DiffNameStatusOptions,
    find_copies_harder: bool,
) -> usize {
    left_entries
        .iter()
        .filter(|(path, _)| find_copies_harder || changed_sources.contains(path.as_slice()))
        .filter(|(_, tracked)| options.rename_empty || !is_empty_blob_oid(&tracked.oid))
        .count()
}

fn inexact_copy_dest_count(changes: &[NameStatusEntry], options: &DiffNameStatusOptions) -> usize {
    changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Added)
        .filter(|entry| {
            entry
                .new_oid
                .as_ref()
                .is_some_and(|oid| options.rename_empty || !is_empty_blob_oid(oid))
        })
        .count()
}

fn inexact_copy_limit_outcome(
    changes: &[NameStatusEntry],
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    options: &DiffNameStatusOptions,
) -> InexactCopyLimitOutcome {
    let changed_sources = changed_copy_sources(changes);
    let dest_count = inexact_copy_dest_count(changes, options);
    let source_count = inexact_copy_source_count(
        &changed_sources,
        left_entries,
        options,
        options.find_copies_harder,
    );
    if !rename_limit_exceeded(source_count, dest_count, options.rename_limit) {
        return InexactCopyLimitOutcome::Full;
    }
    if options.find_copies_harder {
        let changed_source_count =
            inexact_copy_source_count(&changed_sources, left_entries, options, false);
        if !rename_limit_exceeded(changed_source_count, dest_count, options.rename_limit) {
            return InexactCopyLimitOutcome::DegradedToChangedSources;
        }
    }
    InexactCopyLimitOutcome::Skip
}

/// Inexact rename detection: pair still-unmatched deleted files with still-
/// unmatched added files by content similarity, replacing the best matches
/// (similarity >= `rename_threshold`) with [`NameStatus::Renamed`].
///
/// Exact renames have already run, so the only `Deleted`/`Added` entries left
/// here are ones with no identical-OID partner. Assignment is greedy by
/// descending score (then by source/destination order for determinism), and
/// each source and destination is used at most once — matching git's
/// `diffcore-rename` behaviour. Empty blobs are never used as a rename source
/// when `rename_empty` is false, mirroring exact detection.
fn detect_inexact_renames(
    changes: Vec<NameStatusEntry>,
    options: &DiffNameStatusOptions,
    fetch_blob: &impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Vec<NameStatusEntry> {
    let threshold = options.rename_threshold;
    // A threshold above 100 can never be met; nothing to do.
    if threshold > 100 {
        return changes;
    }

    let mut deleted_indices = Vec::new();
    let mut added_indices = Vec::new();
    for (idx, entry) in changes.iter().enumerate() {
        match entry.status {
            NameStatus::Deleted => {
                let Some(oid) = entry.old_oid.as_ref() else {
                    continue;
                };
                if !options.rename_empty && is_empty_blob_oid(oid) {
                    continue;
                }
                deleted_indices.push(idx);
            }
            NameStatus::Added => {
                let Some(oid) = entry.new_oid.as_ref() else {
                    continue;
                };
                if !options.rename_empty && is_empty_blob_oid(oid) {
                    continue;
                }
                added_indices.push(idx);
            }
            _ => {}
        }
    }

    if deleted_indices.is_empty() || added_indices.is_empty() {
        return changes;
    }

    // git's `too_many_rename_candidates`: if the rename matrix would exceed a
    // `rename_limit` square, skip inexact detection wholesale (exact-OID renames
    // were already resolved upstream). A non-positive limit is unlimited.
    if rename_limit_exceeded(
        deleted_indices.len(),
        added_indices.len(),
        options.rename_limit,
    ) {
        return changes;
    }

    // Fetch blob bytes only after the cap check, so a low rename limit avoids
    // both the quadratic scoring matrix and the broad source/destination reads.
    let mut deleted: Vec<(usize, Arc<[u8]>)> = Vec::new();
    for idx in deleted_indices {
        let Some(oid) = changes[idx].old_oid.as_ref() else {
            continue;
        };
        if let Some(bytes) = fetch_blob(oid) {
            deleted.push((idx, bytes));
        }
    }
    let mut added: Vec<(usize, Arc<[u8]>)> = Vec::new();
    for idx in added_indices {
        let Some(oid) = changes[idx].new_oid.as_ref() else {
            continue;
        };
        if let Some(bytes) = fetch_blob(oid) {
            added.push((idx, bytes));
        }
    }
    if deleted.is_empty() || added.is_empty() {
        return changes;
    }

    let mut src_used = vec![false; deleted.len()];
    let mut dst_used = vec![false; added.len()];
    // destination changes-index -> (source changes-index, score).
    let mut rename_of: BTreeMap<usize, (usize, u8)> = BTreeMap::new();

    // Basename pre-pass (git's `find_basename_matches`): before the global
    // matrix, pair unique-basename src/dst at the stricter basename score, so a
    // same-basename rename wins over a globally-more-similar different basename.
    // git only does this for pure rename detection (`!want_copies`); when copies
    // are also wanted it culls differently and skips the basename heuristic.
    if !options.detect_copies {
        let src_paths: Vec<&[u8]> = deleted
            .iter()
            .map(|(idx, _)| &changes[*idx].path[..])
            .collect();
        let dst_paths: Vec<&[u8]> = added
            .iter()
            .map(|(idx, _)| &changes[*idx].path[..])
            .collect();
        let basename_pairs = basename_rename_matches(
            &src_paths,
            &dst_paths,
            &src_used,
            &dst_used,
            threshold,
            |si, di| Some(blob_similarity(&deleted[si].1, &added[di].1)),
        );
        for (si, di, score) in basename_pairs {
            src_used[si] = true;
            dst_used[di] = true;
            rename_of.insert(added[di].0, (deleted[si].0, score));
        }
    }

    // Score every remaining (delete, add) pair; keep only those meeting the
    // threshold.
    let mut pairs: Vec<ScoredPair> = Vec::new();
    for (si, (_, src_bytes)) in deleted.iter().enumerate() {
        if src_used[si] {
            continue;
        }
        for (di, (_, dst_bytes)) in added.iter().enumerate() {
            if dst_used[di] {
                continue;
            }
            let score = blob_similarity(src_bytes, dst_bytes);
            if score >= threshold {
                pairs.push(ScoredPair {
                    src: si,
                    dst: di,
                    score,
                });
            }
        }
    }
    // Best score first; ties broken by source then destination order so the
    // result is deterministic regardless of input ordering.
    pairs.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });

    for pair in pairs {
        if src_used[pair.src] || dst_used[pair.dst] {
            continue;
        }
        src_used[pair.src] = true;
        dst_used[pair.dst] = true;
        let src_change_idx = deleted[pair.src].0;
        let dst_change_idx = added[pair.dst].0;
        rename_of.insert(dst_change_idx, (src_change_idx, pair.score));
    }

    if rename_of.is_empty() {
        return changes;
    }

    // Snapshot the source (delete) entries' metadata before we consume them, so
    // each renamed destination can carry the correct old path/mode/oid.
    let consumed_sources: BTreeSet<usize> =
        rename_of.values().map(|(src_idx, _)| *src_idx).collect();
    let source_meta: BTreeMap<usize, RenameSourceMeta> = consumed_sources
        .iter()
        .map(|&src_idx| {
            let src = &changes[src_idx];
            (
                src_idx,
                RenameSourceMeta {
                    path: src.path.clone(),
                    mode: src.old_mode,
                    oid: src.old_oid,
                },
            )
        })
        .collect();

    let mut result = Vec::with_capacity(changes.len());
    for (idx, entry) in changes.into_iter().enumerate() {
        if consumed_sources.contains(&idx) {
            // This delete became the source of a rename; drop it.
            continue;
        }
        if let Some((src_idx, score)) = rename_of.get(&idx) {
            // The destination becomes a rename from the matched source. Pull the
            // old-side metadata from the snapshot; the new-side metadata stays as
            // the destination's.
            let meta = source_meta
                .get(src_idx)
                .cloned()
                .unwrap_or(RenameSourceMeta {
                    path: BString::default(),
                    mode: None,
                    oid: None,
                });
            result.push(NameStatusEntry {
                status: NameStatus::Renamed(*score),
                path: entry.path,
                old_path: Some(meta.path),
                old_mode: meta.mode,
                new_mode: entry.new_mode,
                old_oid: meta.oid,
                new_oid: entry.new_oid,
            });
            continue;
        }
        result.push(entry);
    }

    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

/// Inexact copy detection: for each still-`Added` file, find the most similar
/// candidate *source* on the left side (similarity >= `copy_threshold`) and, if
/// found, report it as a [`NameStatus::Copied`]. The source is not removed
/// (copies leave the original in place).
///
/// Candidate sources follow the same rule as exact copy detection: with
/// `find_copies_harder` every left-side path is eligible; otherwise only paths
/// that were themselves changed (deleted or modified) on this diff. Exact copies
/// have already run, so any remaining `Added` here had no identical-OID source.
fn detect_inexact_copies(
    changes: Vec<NameStatusEntry>,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    options: &DiffNameStatusOptions,
    fetch_blob: &impl Fn(&ObjectId) -> Option<Arc<[u8]>>,
) -> Vec<NameStatusEntry> {
    let threshold = options.copy_threshold;
    if threshold > 100 {
        return changes;
    }

    let changed_sources = changed_copy_sources(&changes);
    let limit_outcome = inexact_copy_limit_outcome(&changes, left_entries, options);
    if limit_outcome == InexactCopyLimitOutcome::Skip {
        return changes;
    }
    let find_copies_harder = options.find_copies_harder
        && limit_outcome != InexactCopyLimitOutcome::DegradedToChangedSources;
    let mut source_candidates: Vec<(Vec<u8>, &TrackedEntry)> = Vec::new();
    for (path, tracked) in left_entries {
        if !(find_copies_harder || changed_sources.contains(path.as_slice())) {
            continue;
        }
        if !options.rename_empty && is_empty_blob_oid(&tracked.oid) {
            continue;
        }
        source_candidates.push((path.clone(), tracked));
    }
    if source_candidates.is_empty() {
        return changes;
    }

    let mut sources: Vec<(Vec<u8>, &TrackedEntry, Arc<[u8]>)> = Vec::new();
    for (path, tracked) in source_candidates {
        if let Some(bytes) = fetch_blob(&tracked.oid) {
            sources.push((path, tracked, bytes));
        }
    }
    if sources.is_empty() {
        return changes;
    }

    let mut result = Vec::with_capacity(changes.len());
    for entry in changes {
        if entry.status != NameStatus::Added {
            result.push(entry);
            continue;
        }
        let Some(new_oid) = entry.new_oid.as_ref() else {
            result.push(entry);
            continue;
        };
        let Some(dst_bytes) = fetch_blob(new_oid) else {
            result.push(entry);
            continue;
        };

        // Pick the best-scoring source path that meets the threshold. Ties are
        // broken by path order (BTreeMap iteration is sorted) so the choice is
        // deterministic.
        let mut best: Option<(usize, u8)> = None;
        for (i, (src_path, _, src_bytes)) in sources.iter().enumerate() {
            if src_path.as_slice() == entry.path.as_bytes() {
                continue;
            }
            let score = blob_similarity(src_bytes, &dst_bytes);
            if score < threshold {
                continue;
            }
            match best {
                Some((_, best_score)) if best_score >= score => {}
                _ => best = Some((i, score)),
            }
        }

        if let Some((src_idx, score)) = best {
            let (src_path, src_tracked, _) = &sources[src_idx];
            result.push(NameStatusEntry {
                status: NameStatus::Copied(score),
                path: entry.path,
                old_path: Some(src_path.clone().into()),
                old_mode: Some(src_tracked.mode),
                new_mode: entry.new_mode,
                old_oid: Some(src_tracked.oid),
                new_oid: entry.new_oid,
            });
        } else {
            result.push(entry);
        }
    }
    result.sort_by(|left, right| diff_entry_sort_path(left).cmp(diff_entry_sort_path(right)));
    result
}

fn is_empty_blob_oid(oid: &ObjectId) -> bool {
    object_id_for_bytes(oid.format(), "blob", b"").is_ok_and(|empty| empty == *oid)
}

// ===========================================================================
// Content similarity (the engine for inexact `-M`/`-C` rename/copy detection).
//
// This mirrors upstream git's similarity estimate from `diffcore-delta.c`
// (the span-hash counting) and `diffcore-rename.c` (the score formula), so the
// `R<score>`/`C<score>` we emit match git's percentages.
//
// The metric, precisely:
//
//   1. Each blob is broken into *spans*. Starting at a byte, we accumulate a
//      rolling hash of the bytes and end the span at the first `\n` (inclusive)
//      or once the span reaches `MAX_SPAN_BYTES` (64) bytes, whichever comes
//      first. (The 64-byte cap keeps a file with no/few newlines — e.g. a
//      binary blob or one very long line — from collapsing into a single span,
//      so similarity still tracks shared substrings.) Each span yields a
//      `(hash, byte_count)` pair, where `byte_count` is the span's length in
//      bytes. This is the exact loop git uses in `hash_chars()`.
//
//   2. The two blobs' spans are reduced to multisets keyed by hash: for each
//      hash we keep the total number of bytes spanned by entries with that
//      hash, on each side. `common_bytes` is then the sum over all hashes of
//      `min(bytes_on_src, bytes_on_dst)` — the bytes that exist on both sides.
//      This is git's `src_copied`.
//
//   3. The score is `common_bytes / max(size_src, size_dst)`, scaled to a
//      percentage and rounded to the nearest integer:
//
//          score% = round(common_bytes * 100 / max(size_src, size_dst))
//
//      git computes an internal score `src_copied * MAX_SCORE / max_size` with
//      `MAX_SCORE == 60000` and reports `round(score * 100 / MAX_SCORE)`; that
//      is algebraically the same rounded percentage, which we compute directly
//      to avoid intermediate precision loss.
//
// Edge cases match git: two empty blobs are 100% similar (identical content);
// an empty blob vs a non-empty one is 0%. Equal byte buffers are always 100%.

/// Maximum number of bytes in a single similarity span before it is force-cut.
///
/// git uses 64 (`hash_chars()` breaks a span once `++chunks >= 64`).
const MAX_SPAN_BYTES: usize = 64;

/// Compute the content similarity of two blobs as an integer percentage in
/// `0..=100`, using git's span-hash counting metric (see the module comment
/// above for the exact definition).
///
/// The result is symmetric (`blob_similarity(a, b) == blob_similarity(b, a)`)
/// because the score divides the common-byte count by the larger of the two
/// sizes. Byte-identical blobs return `100`; a non-empty blob compared against
/// an empty one returns `0`; two empty blobs return `100`.
///
/// This is the same number git prints as `similarity index N%` and uses to
/// decide `-M`/`-C` rename and copy detection.
pub fn blob_similarity(a: &[u8], b: &[u8]) -> u8 {
    // Fast paths that also pin down the empty-blob conventions.
    if a == b {
        return 100;
    }
    let max_size = a.len().max(b.len());
    if max_size == 0 {
        // Both empty (and not caught by `a == b` only if both are empty, which
        // they are here) -> identical.
        return 100;
    }

    let src = span_hash_counts(a, blob_is_text(a));
    let dst = span_hash_counts(b, blob_is_text(b));
    let common = common_span_bytes(&src, &dst);

    // Match git's diffcore-rename integer math exactly. git computes an internal
    // score `src_copied * MAX_SCORE / max_size` (MAX_SCORE == 60000) with integer
    // truncation, then reports the similarity index as `score * 100 / MAX_SCORE`,
    // truncated again. This two-step truncation -- *not* a single rounded
    // `common * 100 / max_size` -- is what yields git's exact percentages: e.g.
    // common=4, max_size=6 gives 4*60000/6=40000 then 40000*100/60000=66 (git's
    // `R066`), whereas a rounded single step would give 67.
    const MAX_SCORE: u64 = 60000;
    let internal = (common as u64 * MAX_SCORE) / max_size as u64;
    let score = internal * 100 / MAX_SCORE;
    score.min(100) as u8
}

/// The basename of a slash-separated path: the portion after the last `/`
/// (git's `get_basename`).
pub fn path_basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&byte| byte == b'/') {
        Some(slash) => &path[slash + 1..],
        None => path,
    }
}

/// The stricter score a basename match must reach: git's `min_basename_score`
/// with the default `GIT_BASENAME_FACTOR` of 0.5, i.e. halfway between the
/// rename threshold and 100%. (For the default 50% threshold this is 75%.)
pub fn basename_min_score(threshold: u8) -> u8 {
    let threshold = threshold.min(100);
    threshold + (100 - threshold) / 2
}

/// git's `find_basename_matches`: among the still-unmatched rename sources and
/// destinations, pair those whose basename is UNIQUE on *both* sides and whose
/// similarity meets [`basename_min_score`]. Returns the `(src_local, dst_local,
/// score)` pairings to apply *before* the full O(n·m) similarity matrix, so a
/// same-basename rename wins over a globally-more-similar different-basename
/// candidate (diffcore-rename.c).
///
/// `src_paths`/`dst_paths` are the candidate paths, indexed in parallel with the
/// `src_used`/`dst_used` flags (entries already consumed by exact-OID matching).
/// `similarity(src_local, dst_local)` returns the blob similarity for a pair, or
/// `None` when a blob is unreadable / ineligible. Only unique basenames are
/// considered: git's plain-diff path has no directory-rename fallback, so an
/// ambiguous basename is skipped entirely.
pub fn basename_rename_matches(
    src_paths: &[&[u8]],
    dst_paths: &[&[u8]],
    src_used: &[bool],
    dst_used: &[bool],
    threshold: u8,
    mut similarity: impl FnMut(usize, usize) -> Option<u8>,
) -> Vec<(usize, usize, u8)> {
    let min_score = basename_min_score(threshold);
    // basename -> Some(unique local index), or None once a second candidate with
    // the same basename appears (ambiguous).
    let mut src_by_base: HashMap<&[u8], Option<usize>> = HashMap::new();
    for (si, path) in src_paths.iter().enumerate() {
        if src_used.get(si).copied().unwrap_or(false) {
            continue;
        }
        src_by_base
            .entry(path_basename(path))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(si));
    }
    let mut dst_by_base: HashMap<&[u8], Option<usize>> = HashMap::new();
    for (di, path) in dst_paths.iter().enumerate() {
        if dst_used.get(di).copied().unwrap_or(false) {
            continue;
        }
        dst_by_base
            .entry(path_basename(path))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(di));
    }
    let mut matches = Vec::new();
    let mut dst_taken = vec![false; dst_paths.len()];
    for (si, path) in src_paths.iter().enumerate() {
        if src_used.get(si).copied().unwrap_or(false) {
            continue;
        }
        let base = path_basename(path);
        // Both basenames must be unique among the remaining candidates.
        let Some(Some(src_idx)) = src_by_base.get(base).copied() else {
            continue;
        };
        if src_idx != si {
            continue;
        }
        let Some(Some(dst_idx)) = dst_by_base.get(base).copied() else {
            continue;
        };
        if dst_used.get(dst_idx).copied().unwrap_or(false) || dst_taken[dst_idx] {
            continue;
        }
        let Some(score) = similarity(si, dst_idx) else {
            continue;
        };
        if score < min_score {
            continue;
        }
        dst_taken[dst_idx] = true;
        matches.push((si, dst_idx, score));
    }
    matches
}

/// Break `data` into spans and return, per span hash, the total number of bytes
/// covered by spans with that hash. Spans end at a newline (inclusive) or once
/// they reach [`MAX_SPAN_BYTES`] bytes — exactly git's `hash_chars()` loop.
///
/// The returned map is `hash -> total_span_bytes`. Summing all values yields
/// `data.len()`, so the byte accounting is exact.
fn span_hash_counts(data: &[u8], is_text: bool) -> BTreeMap<u64, usize> {
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    let mut idx = 0usize;
    let len = data.len();
    while idx < len {
        // Roll a hash over the bytes of this span. The mixing mirrors git's
        // two-accumulator scheme from `diffcore-delta.c`; the exact constants do
        // not matter for correctness (any good per-span hash works), only that
        // identical spans collide and distinct spans rarely do.
        let mut accum1: u32 = 0;
        let mut accum2: u32 = 0;
        let mut span_len = 0usize;
        loop {
            let c = data[idx] as u32;
            idx += 1;
            // Ignore CR in a CRLF sequence for text blobs, so a file that only
            // differs by LF<->CRLF is still scored as (near-)identical — git's
            // `hash_chars()` does the same, which is what makes a CRLF-only
            // rename detectable.
            if is_text && c == u32::from(b'\r') && idx < len && data[idx] == b'\n' {
                continue;
            }
            span_len += 1;
            accum1 = (accum1 << 7) ^ (accum2 >> 25);
            accum2 = (accum2 << 7) ^ (accum1 >> 25);
            accum1 = accum1.wrapping_add(c);
            let newline = c == u32::from(b'\n');
            if span_len >= MAX_SPAN_BYTES || newline || idx >= len {
                break;
            }
        }
        // Fold the two accumulators (and the span length) into one 64-bit key.
        // Including the length keeps spans of different lengths from colliding
        // when their rolling-hash states happen to coincide.
        let hash = ((accum1 as u64) << 32) ^ (accum2 as u64) ^ ((span_len as u64) << 1);
        *counts.entry(hash).or_insert(0) += span_len;
    }
    counts
}

/// Sum, over every hash present in both maps, the smaller of the two byte
/// counts. This is git's `src_copied`: the number of bytes that appear on both
/// sides (counting multiplicity via the per-hash byte totals).
/// git `diffcore_count_changes()`: span-hash byte accounting between two
/// blobs. Returns `(src_copied, literal_added)` — the bytes of `src` that
/// survive into `dst`, and the bytes of `dst` not accounted for by `src`.
/// `--dirstat`'s default "changes" damage is
/// `(src.len() - src_copied) + literal_added`.
pub fn count_changes(src: &[u8], dst: &[u8]) -> (usize, usize) {
    let src_counts = span_hash_counts(src, blob_is_text(src));
    let dst_counts = span_hash_counts(dst, blob_is_text(dst));
    let copied = common_span_bytes(&src_counts, &dst_counts);
    (copied, dst.len() - copied)
}

/// Whether a blob is treated as text for span hashing (git's
/// `diff_filespec_is_binary` / `buffer_is_binary`): a NUL byte within the first
/// 8000 bytes marks it binary, in which case CRs are hashed literally.
fn blob_is_text(data: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    !data.iter().take(FIRST_FEW_BYTES).any(|&byte| byte == 0)
}

fn common_span_bytes(src: &BTreeMap<u64, usize>, dst: &BTreeMap<u64, usize>) -> usize {
    let mut common = 0usize;
    // Iterate the smaller map for a few less lookups.
    let (small, large) = if src.len() <= dst.len() {
        (src, dst)
    } else {
        (dst, src)
    };
    for (hash, small_bytes) in small {
        if let Some(large_bytes) = large.get(hash) {
            common += (*small_bytes).min(*large_bytes);
        }
    }
    common
}

fn diff_entry_sort_path(entry: &NameStatusEntry) -> &[u8] {
    // git's diffcore re-inserts rename/copy pairs at their *destination*'s
    // position, so the queue (raw, numstat, stat, ...) sorts by the new path.
    entry.path.as_bytes()
}

fn mark_unstaged_worktree_oids_unresolved(
    changes: Vec<NameStatusEntry>,
    index_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Vec<NameStatusEntry> {
    changes
        .into_iter()
        .map(|mut entry| {
            let worktree_entry = worktree_entries.get(entry.path.as_bytes());
            if worktree_entry != index_entries.get(entry.path.as_bytes()) {
                entry.new_oid = None;
            }
            entry
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedEntry {
    pub(crate) mode: u32,
    pub(crate) oid: ObjectId,
}

/// A path-keyed map of tracked entries: one flattened side of a tree (or index/
/// worktree) snapshot.
type TrackedEntryMap = BTreeMap<Vec<u8>, TrackedEntry>;

/// The `(left, right)` sides produced by a tree-vs-tree comparison.
type TrackedEntryPair = (TrackedEntryMap, TrackedEntryMap);

struct IndexSnapshot {
    entries: BTreeMap<Vec<u8>, TrackedEntry>,
    stat_cache: IndexStatCache,
    missing_skip_worktree_entries: BTreeMap<Vec<u8>, TrackedEntry>,
    present_skip_worktree_entries: BTreeMap<Vec<u8>, TrackedEntry>,
}

fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeMap::new());
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    Ok(index
        .entries
        .into_iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && !entry.is_intent_to_add())
        .map(|entry| {
            (
                entry.path.into_bytes(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect())
}

/// Collect the set of stage-0 paths flagged intent-to-add (`git add -N`) in the
/// index. These diff as new files rather than as modifications of their recorded
/// empty-blob id.
fn read_intent_to_add_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<std::collections::HashSet<Vec<u8>>> {
    let index_path = sley_index::repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(std::collections::HashSet::new());
    }
    let index = expand_sparse_index_for_worktree_diff(
        sley_index::read_repository_index(git_dir, format)?,
        git_dir,
        format,
    )?;
    Ok(index
        .entries
        .iter()
        .filter(|entry| entry.stage() == sley_index::Stage::Normal && entry.is_intent_to_add())
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect())
}

fn read_index_snapshot(git_dir: &Path, format: ObjectFormat) -> Result<IndexSnapshot> {
    let index_path = sley_index::repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexSnapshot {
                entries: BTreeMap::new(),
                stat_cache: IndexStatCache::default(),
                missing_skip_worktree_entries: BTreeMap::new(),
                present_skip_worktree_entries: BTreeMap::new(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let raw_index = sley_index::read_repository_index(git_dir, format)?;
    let sparse_checkout_active = raw_index.is_sparse()
        || raw_index
            .entries
            .iter()
            .any(|entry| entry.mode == sley_index::SPARSE_DIR_MODE && entry.is_skip_worktree())
        || sparse_checkout_config_enabled(git_dir);
    let index = expand_sparse_index_for_worktree_diff(raw_index, git_dir, format)?;
    let stat_cache =
        IndexStatCache::from_index_mtime(&index, sley_index::file_mtime_parts(&index_metadata));
    let mut entries = BTreeMap::new();
    let mut missing_skip_worktree_entries = BTreeMap::new();
    let mut present_skip_worktree_entries = BTreeMap::new();
    for entry in index.entries {
        let is_skip_worktree = entry.stage() == sley_index::Stage::Normal
            && entry.is_skip_worktree()
            && !entry.is_intent_to_add();
        let path = entry.path.into_bytes();
        let tracked = TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        };
        if is_skip_worktree {
            missing_skip_worktree_entries.insert(path.clone(), tracked.clone());
            if !sparse_checkout_active {
                present_skip_worktree_entries.insert(path.clone(), tracked.clone());
            }
        }
        entries.insert(path, tracked);
    }
    Ok(IndexSnapshot {
        entries,
        stat_cache,
        missing_skip_worktree_entries,
        present_skip_worktree_entries,
    })
}

fn sparse_checkout_config_enabled(git_dir: &Path) -> bool {
    sley_config::GitConfig::read(git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
        == Some(true)
        || sley_config::GitConfig::read(git_dir.join("config.worktree"))
            .ok()
            .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
            == Some(true)
}

trait WorktreeIndexEntry {
    fn git_path(&self) -> &[u8];
    fn stage(&self) -> sley_index::Stage;
    fn mode(&self) -> u32;
    fn oid(&self) -> ObjectId;
    fn size(&self) -> u32;
    fn is_intent_to_add(&self) -> bool;
    fn is_skip_worktree(&self) -> bool;
    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool;
}

impl WorktreeIndexEntry for sley_index::IndexEntry {
    fn git_path(&self) -> &[u8] {
        self.path.as_bytes()
    }

    fn stage(&self) -> sley_index::Stage {
        sley_index::IndexEntry::stage(self)
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> ObjectId {
        self.oid
    }

    fn size(&self) -> u32 {
        self.size
    }

    fn is_intent_to_add(&self) -> bool {
        sley_index::IndexEntry::is_intent_to_add(self)
    }

    fn is_skip_worktree(&self) -> bool {
        sley_index::IndexEntry::is_skip_worktree(self)
    }

    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool {
        stat_cache.reusable_index_entry(self, metadata).is_some()
    }
}

impl WorktreeIndexEntry for sley_index::IndexEntryRef<'_> {
    fn git_path(&self) -> &[u8] {
        self.path
    }

    fn stage(&self) -> sley_index::Stage {
        sley_index::IndexEntryRef::stage(self)
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn oid(&self) -> ObjectId {
        self.oid
    }

    fn size(&self) -> u32 {
        self.size
    }

    fn is_intent_to_add(&self) -> bool {
        sley_index::IndexEntryRef::is_intent_to_add(self)
    }

    fn is_skip_worktree(&self) -> bool {
        sley_index::IndexEntryRef::is_skip_worktree(self)
    }

    fn reusable_with(&self, stat_cache: &IndexStatCache, metadata: &fs::Metadata) -> bool {
        stat_cache.reusable_index_entry_ref(self, metadata)
    }
}

fn tracked_entry_from_index(entry: &impl WorktreeIndexEntry) -> TrackedEntry {
    TrackedEntry {
        mode: entry.mode(),
        oid: entry.oid(),
    }
}

fn head_tree_entries(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let refs = FileRefStore::new(git_dir, format);
    let Some(head) = refs.read_ref("HEAD")? else {
        return Ok(BTreeMap::new());
    };
    let commit_oid = match head {
        RefTarget::Direct(oid) => Some(oid),
        RefTarget::Symbolic(name) => match refs.read_ref(&name)? {
            Some(RefTarget::Direct(oid)) => Some(oid),
            _ => None,
        },
    };
    let Some(commit_oid) = commit_oid else {
        return Ok(BTreeMap::new());
    };
    let object = db.read_object(&commit_oid)?;
    if object.object_type != ObjectType::Commit {
        return Err(GitError::InvalidObject(format!(
            "HEAD {commit_oid} is not a commit"
        )));
    }
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, Vec::new(), &mut entries)?;
    Ok(entries)
}

/// Flatten `tree_oid` into `entries` (keyed by `prefix`-rooted full paths),
/// adapting the canonical [`flatten_tree`] tuples into [`TrackedEntry`].
///
/// `flatten_tree` flattens from an empty prefix; each of its paths is rejoined
/// under `prefix` with [`join_tree_path`], reproducing the recursive
/// prefix-building this helper previously did inline. Used by the full
/// (non-pruned) flatten paths: `--find-copies-harder` and the changed-subtree
/// add/delete sides of the simultaneous diff walk.
fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    for (rel_path, (mode, oid)) in flatten_tree(db, format, tree_oid)? {
        let path = join_tree_path(&prefix, &rel_path);
        entries.insert(path, TrackedEntry { mode, oid });
    }
    Ok(())
}

/// Git's mode value for a subtree (directory) entry inside a tree object.
pub(crate) const TREE_ENTRY_MODE: u32 = 0o040000;

/// Read `tree_oid` and parse it as a tree, erroring if the object is some other
/// type. Shared by the simultaneous tree-diff walk so both sides validate the
/// object type identically to [`collect_tree_entries`].
fn read_tree_object(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Tree> {
    let object = db.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {tree_oid}, found {}",
            object.object_type.as_str()
        )));
    }
    Tree::parse(format, &object.body)
}

/// Append `name` to `prefix` with a `/` separator (mirroring the path
/// construction in [`collect_tree_entries`]), returning the joined path.
fn join_tree_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(prefix.len() + 1 + name.len());
    path.extend_from_slice(prefix);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Fully flatten both trees into independent `left`/`right` maps (every blob on
/// each side, no pruning). Used only on the `--find-copies-harder` path, where
/// copy detection may reach into otherwise-unchanged subtrees for a source.
pub(crate) fn collect_full_tree_pair(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<TrackedEntryPair> {
    let mut left = BTreeMap::new();
    collect_tree_entries(db, format, left_tree, Vec::new(), &mut left)?;
    let mut right = BTreeMap::new();
    collect_tree_entries(db, format, right_tree, Vec::new(), &mut right)?;
    Ok((left, right))
}

/// Walk two trees *simultaneously*, collecting into `left` and `right` only the
/// blob entries that differ between the two sides — every entry that is present
/// and byte-identical (same mode + same OID) on both sides is omitted, and any
/// subtree whose OID is identical on both sides is skipped wholesale without
/// being read or recursed into. This is the core optimization git relies on to
/// make tree diffs cheap: equal subtrees are pruned in O(1).
///
/// The resulting `left`/`right` maps are exactly the subset of the fully
/// flattened maps (as produced by [`collect_tree_entries`]) restricted to the
/// paths that participate in an Added/Deleted/Modified change. Because
/// [`raw_name_status_changes`] emits nothing for a path that is identical on both
/// sides, diffing these pruned maps yields byte-identical name-status output to
/// diffing the full maps. (Callers that need the *complete* left map — i.e.
/// `--find-copies-harder`, where an unchanged file may be a copy source — must
/// still use [`collect_tree_entries`]; see the tree-diff entry points.)
pub(crate) fn changed_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
) -> Result<TrackedEntryPair> {
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    // Identical root trees produce no changes at all and need not be read.
    if left_tree != right_tree {
        diff_tree_pair(
            db,
            format,
            left_tree,
            right_tree,
            &[],
            &mut left,
            &mut right,
        )?;
    }
    Ok((left, right))
}

/// Recursively diff two subtrees rooted at `prefix`, appending differing blob
/// entries to `left` / `right`. Invariant: the two OIDs are already known to
/// differ (identical subtrees are pruned by the caller before recursing).
fn diff_tree_pair(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    prefix: &[u8],
    left: &mut BTreeMap<Vec<u8>, TrackedEntry>,
    right: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let left_entries = read_tree_object(db, format, left_tree)?.entries;
    let right_entries = read_tree_object(db, format, right_tree)?.entries;

    // Index the right side by name so the union of names can be walked without
    // relying on git's directory-aware entry ordering. (Iterating the union of
    // names, rather than a positional merge, keeps correctness independent of
    // entry order.)
    let mut right_by_name: HashMap<&[u8], &TreeEntry> = HashMap::with_capacity(right_entries.len());
    for entry in &right_entries {
        right_by_name.insert(entry.name.as_bytes(), entry);
    }

    for left_entry in &left_entries {
        match right_by_name.remove(left_entry.name.as_bytes()) {
            Some(right_entry) => {
                merge_tree_entry(
                    db,
                    format,
                    prefix,
                    Some(left_entry),
                    Some(right_entry),
                    left,
                    right,
                )?;
            }
            None => {
                merge_tree_entry(db, format, prefix, Some(left_entry), None, left, right)?;
            }
        }
    }
    // Names only present on the right are pure additions.
    for right_entry in &right_entries {
        if right_by_name.contains_key(right_entry.name.as_bytes()) {
            merge_tree_entry(db, format, prefix, None, Some(right_entry), left, right)?;
        }
    }
    Ok(())
}

/// Reconcile a single name that may appear on the left side, the right side, or
/// both, recording any resulting blob change(s) into `left` / `right`. This
/// reproduces exactly the union-of-flattened-maps semantics:
///
/// * tree vs tree with equal OID -> pruned (no read, no recursion);
/// * tree vs tree with differing OID -> recurse;
/// * blob vs blob, equal mode+OID -> unchanged, emitted nowhere;
/// * blob vs blob, differing mode or OID -> both sides recorded (a Modify);
/// * a tree on one side and a non-tree on the other (or a name present on only
///   one side) -> the flattened paths differ (`name/...` vs `name`), so the two
///   are unrelated: the tree side is flattened wholesale and the blob side is
///   recorded independently (an Add and/or a Delete).
fn merge_tree_entry(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    prefix: &[u8],
    left_entry: Option<&TreeEntry>,
    right_entry: Option<&TreeEntry>,
    left: &mut BTreeMap<Vec<u8>, TrackedEntry>,
    right: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    let left_is_tree = left_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);
    let right_is_tree = right_entry.is_some_and(|entry| entry.mode == TREE_ENTRY_MODE);

    if let (Some(left_entry), Some(right_entry)) = (left_entry, right_entry) {
        if left_is_tree && right_is_tree {
            // Two subtrees under the same name: prune if identical, else recurse.
            if left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            return diff_tree_pair(
                db,
                format,
                &left_entry.oid,
                &right_entry.oid,
                &path,
                left,
                right,
            );
        }
        if !left_is_tree && !right_is_tree {
            // Two blobs under the same name. Identical mode+OID means unchanged
            // (nothing emitted); otherwise both sides are recorded so the diff
            // sees a Modify, matching the full-map `left != right` comparison.
            if left_entry.mode == right_entry.mode && left_entry.oid == right_entry.oid {
                return Ok(());
            }
            let path = join_tree_path(prefix, left_entry.name.as_bytes());
            left.insert(
                path.clone(),
                TrackedEntry {
                    mode: left_entry.mode,
                    oid: left_entry.oid,
                },
            );
            right.insert(
                path,
                TrackedEntry {
                    mode: right_entry.mode,
                    oid: right_entry.oid,
                },
            );
            return Ok(());
        }
        // Mixed: tree on one side, blob on the other. Their flattened paths
        // never collide, so handle each side as if the name existed only there.
    }

    // Left side (if any): record as deletions.
    if let Some(left_entry) = left_entry {
        let path = join_tree_path(prefix, left_entry.name.as_bytes());
        if left_is_tree {
            collect_tree_entries(db, format, &left_entry.oid, path, left)?;
        } else {
            left.insert(
                path,
                TrackedEntry {
                    mode: left_entry.mode,
                    oid: left_entry.oid,
                },
            );
        }
    }
    // Right side (if any): record as additions.
    if let Some(right_entry) = right_entry {
        let path = join_tree_path(prefix, right_entry.name.as_bytes());
        if right_is_tree {
            collect_tree_entries(db, format, &right_entry.oid, path, right)?;
        } else {
            right.insert(
                path,
                TrackedEntry {
                    mode: right_entry.mode,
                    oid: right_entry.oid,
                },
            );
        }
    }
    Ok(())
}

fn index_gitlinks(index: &BTreeMap<Vec<u8>, TrackedEntry>) -> BTreeMap<Vec<u8>, ObjectId> {
    index
        .iter()
        .filter(|(_, entry)| sley_index::is_gitlink(entry.mode))
        .map(|(path, entry)| (path.clone(), entry.oid))
        .collect()
}

fn candidate_path_set<'a>(candidate_paths: impl Iterator<Item = &'a Vec<u8>>) -> BTreeSet<Vec<u8>> {
    candidate_paths.cloned().collect()
}

fn worktree_entries_for_path_set(
    worktree_root: &Path,
    format: ObjectFormat,
    candidates: &BTreeSet<Vec<u8>>,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
    missing_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
    present_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    worktree_entries_for_unique_paths(
        worktree_root,
        format,
        candidates.iter(),
        index_gitlinks,
        stat_cache,
        missing_skip_worktree_entries,
        present_skip_worktree_entries,
    )
}

fn worktree_entries_for_unique_paths<'a>(
    worktree_root: &Path,
    format: ObjectFormat,
    candidates: impl Iterator<Item = &'a Vec<u8>>,
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
    missing_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
    present_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    for git_path in candidates {
        if let Some(entry) = worktree_entry_for_path(
            worktree_root,
            format,
            git_path,
            index_gitlinks,
            stat_cache,
            missing_skip_worktree_entries,
            present_skip_worktree_entries,
        )? {
            entries.insert(git_path.clone(), entry);
        }
    }
    Ok(entries)
}

fn worktree_entry_for_path(
    worktree_root: &Path,
    format: ObjectFormat,
    git_path: &[u8],
    index_gitlinks: &BTreeMap<Vec<u8>, ObjectId>,
    stat_cache: Option<&IndexStatCache>,
    missing_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
    present_skip_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
) -> Result<Option<TrackedEntry>> {
    if let Some(entry) = present_skip_worktree_entries.and_then(|entries| {
        entries
            .get(git_path)
            .cloned()
            .filter(|entry| !sley_index::is_gitlink(entry.mode))
    }) {
        return Ok(Some(entry));
    }
    let path = worktree_path_for_repo_path(worktree_root, git_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if is_missing_worktree_path_error(&err) => {
            return Ok(missing_skip_worktree_entries.and_then(|entries| {
                entries
                    .get(git_path)
                    .cloned()
                    .filter(|entry| !sley_index::is_gitlink(entry.mode))
            }));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let file_type = metadata.file_type();
    if let Some(staged_oid) = index_gitlinks.get(git_path)
        && metadata.is_dir()
    {
        let oid = gitlink_head_oid(&path, format).unwrap_or(*staged_oid);
        return Ok(Some(TrackedEntry {
            mode: sley_index::GITLINK_MODE,
            oid,
        }));
    }
    if metadata.is_dir() {
        if let Some(oid) = gitlink_head_oid(&path, format) {
            return Ok(Some(TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            }));
        }
        return Ok(None);
    }
    if !(metadata.is_file() || file_type.is_symlink()) {
        return Ok(None);
    }
    if let Some(entry) = stat_cache.and_then(|cache| cache.reusable_entry(git_path, &metadata)) {
        return Ok(Some(tracked_entry_from_index(entry)));
    }
    Ok(Some(classify_worktree_entry(&path, &metadata, format)?))
}

fn index_worktree_change_for_entry(
    path: &Path,
    format: ObjectFormat,
    index_entry: &impl WorktreeIndexEntry,
    stat_cache: &IndexStatCache,
    stat_clean_validator: &mut Option<&mut IndexWorktreeStatCleanValidator<'_>>,
) -> Result<Option<NameStatusEntry>> {
    let git_path = index_entry.git_path();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if is_missing_worktree_path_error(&err) && index_entry.is_skip_worktree() => {
            return Ok(None);
        }
        Err(err) if is_missing_worktree_path_error(&err) => {
            return Ok(Some(index_worktree_deleted_entry(index_entry)));
        }
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    let file_type = metadata.file_type();
    let right = if metadata.is_dir() {
        if sley_index::is_gitlink(index_entry.mode()) {
            let oid = gitlink_head_oid(path, format).unwrap_or(index_entry.oid());
            Some(TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            })
        } else {
            gitlink_head_oid(path, format).map(|oid| TrackedEntry {
                mode: sley_index::GITLINK_MODE,
                oid,
            })
        }
    } else if metadata.is_file() || file_type.is_symlink() {
        let validated = if let Some(validator) = stat_clean_validator.as_deref_mut() {
            validator(
                IndexWorktreeValidationEntry {
                    path: index_entry.git_path(),
                    mode: index_entry.mode(),
                    oid: index_entry.oid(),
                    size: index_entry.size(),
                },
                path,
                &metadata,
            )?
        } else {
            None
        };
        if index_entry.reusable_with(stat_cache, &metadata) {
            return Ok(None);
        }
        Some(match validated {
            Some(entry) => TrackedEntry {
                mode: entry.mode,
                oid: entry.oid,
            },
            None => classify_worktree_entry(path, &metadata, format)?,
        })
    } else {
        None
    };
    let Some(right) = right else {
        return Ok(Some(index_worktree_deleted_entry(index_entry)));
    };
    let left = tracked_entry_from_index(index_entry);
    if right == left {
        return Ok(None);
    }
    Ok(Some(NameStatusEntry {
        status: modify_or_type_change(left.mode, right.mode),
        path: git_path.to_vec().into(),
        old_path: None,
        old_mode: Some(left.mode),
        new_mode: Some(right.mode),
        old_oid: Some(left.oid),
        new_oid: Some(right.oid),
    }))
}

fn index_worktree_deleted_entry(index_entry: &impl WorktreeIndexEntry) -> NameStatusEntry {
    NameStatusEntry {
        status: NameStatus::Deleted,
        path: index_entry.git_path().to_vec().into(),
        old_path: None,
        old_mode: Some(index_entry.mode()),
        new_mode: None,
        old_oid: Some(index_entry.oid()),
        new_oid: None,
    }
}

fn is_missing_worktree_path_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn worktree_blob_cache_for_path_set(
    worktree_root: &Path,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: &BTreeSet<Vec<u8>>,
    odb_backed_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
    options: DiffNameStatusOptions,
) -> Result<HashMap<ObjectId, Arc<[u8]>>> {
    worktree_blob_cache_for_unique_paths(
        worktree_root,
        left_entries,
        right_entries,
        candidate_paths.iter(),
        odb_backed_worktree_entries,
        options,
    )
}

fn worktree_blob_cache_for_unique_paths<'a>(
    worktree_root: &Path,
    left_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    right_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    candidate_paths: impl Iterator<Item = &'a Vec<u8>>,
    odb_backed_worktree_entries: Option<&BTreeMap<Vec<u8>, TrackedEntry>>,
    options: DiffNameStatusOptions,
) -> Result<HashMap<ObjectId, Arc<[u8]>>> {
    if !options.detect_inexact || !(options.detect_renames || options.detect_copies) {
        return Ok(HashMap::new());
    }
        let mut changes =
        raw_name_status_changes_for_unique_paths(left_entries, right_entries, candidate_paths);
    if options.detect_renames {
        changes = detect_exact_renames(changes, left_entries, right_entries, options.rename_empty);
    }
    if options.detect_copies {
        changes = detect_exact_copies(
            changes,
            left_entries,
            right_entries,
            options.find_copies_harder,
            options.rename_empty,
        );
    }
    let has_rename_source = options.detect_renames
        && changes.iter().any(|entry| {
            entry.status == NameStatus::Deleted
                && entry
                    .old_oid
                    .as_ref()
                    .is_some_and(|oid| options.rename_empty || !is_empty_blob_oid(oid))
        });
    let has_copy_source = options.detect_copies
        && (options.find_copies_harder
            || changes
                .iter()
                .any(|entry| matches!(entry.status, NameStatus::Deleted | NameStatus::Modified)));
    if !has_rename_source && !has_copy_source {
        return Ok(HashMap::new());
    }
    let candidate_oids = changes
        .iter()
        .filter(|entry| entry.status == NameStatus::Added)
        .filter_map(|entry| entry.new_oid)
        .filter(|oid| options.rename_empty || !is_empty_blob_oid(oid))
        .collect::<BTreeSet<_>>();
    if candidate_oids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut cache = HashMap::new();
    for (git_path, entry) in right_entries {
        if odb_backed_worktree_entries.is_some_and(|entries| entries.contains_key(git_path)) {
            continue;
        }
        if sley_index::is_gitlink(entry.mode) || !candidate_oids.contains(&entry.oid) {
            continue;
        }
        let path = worktree_path_for_repo_path(worktree_root, git_path);
        let body = if sley_index::is_symlink_mode(entry.mode) {
            symlink_target_bytes(&path)?
        } else {
            fs::read(&path)?
        };
        cache
            .entry(entry.oid)
            .or_insert_with(|| Arc::from(body.into_boxed_slice()));
    }
    Ok(cache)
}

/// A blob fetcher that consults an in-memory `oid -> bytes` cache first (e.g.
/// freshly-read worktree files) and falls back to the object database.
fn cache_or_odb_blob(
    cache: &HashMap<ObjectId, Arc<[u8]>>,
    db: &FileObjectDatabase,
    oid: &ObjectId,
) -> Option<Arc<[u8]>> {
    if let Some(bytes) = cache.get(oid) {
        return Some(bytes.clone());
    }
    read_blob_bytes(db, oid)
}

#[cfg(unix)]
fn worktree_path_for_repo_path(worktree_root: &Path, path: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let mut out = PathBuf::from(worktree_root);
    out.push(OsStr::from_bytes(path));
    out
}

#[cfg(unix)]
fn worktree_path_for_repo_path_into(out: &mut PathBuf, worktree_root: &Path, path: &[u8]) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    out.clear();
    out.push(worktree_root);
    out.push(OsStr::from_bytes(path));
}

#[cfg(not(unix))]
fn worktree_path_for_repo_path(worktree_root: &Path, path: &[u8]) -> PathBuf {
    worktree_root.join(repo_path_to_path(path))
}

#[cfg(not(unix))]
fn worktree_path_for_repo_path_into(out: &mut PathBuf, worktree_root: &Path, path: &[u8]) {
    out.clear();
    out.push(worktree_root);
    out.push(repo_path_to_path(path));
}

#[cfg(not(unix))]
fn repo_path_to_path(path: &[u8]) -> PathBuf {
    let mut out = PathBuf::new();
    for component in String::from_utf8_lossy(path).split('/') {
        if !component.is_empty() {
            out.push(component);
        }
    }
    out
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// Read a symbolic link's target as git stores it: the raw target path bytes,
/// with no trailing newline. This is the "content" of a symlink blob (mode
/// `120000`) — git's `diff_populate_filespec` uses `strbuf_readlink` for a
/// worktree symlink rather than dereferencing it.
#[cfg(unix)]
pub fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let target = fs::read_link(path)?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

/// See the unix variant: the raw symlink target bytes git stores as the blob.
#[cfg(not(unix))]
pub fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
}
/// Flattened tree: repository-relative path -> (mode, blob/symlink/gitlink oid).
pub type MergeEntryMap = BTreeMap<Vec<u8>, (u32, ObjectId)>;
/// Read a tree object (by oid) into a flattened path -> (mode, oid) map,
/// descending into subtrees. The canonical empty tree yields an empty map.
pub fn flatten_tree(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<MergeEntryMap> {
    let mut entries = BTreeMap::new();
    if *tree_oid == empty_tree_oid(format)? {
        return Ok(entries);
    }
    collect_flat_tree(reader, format, tree_oid, Vec::new(), &mut entries)?;
    Ok(entries)
}

pub fn corrupted_cache_tree_error() -> GitError {
    GitError::InvalidFormat("error: corrupted cache-tree has entries not present in index".into())
}

pub fn tree_has_duplicate_leaf_paths(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<bool> {
    if *tree_oid == empty_tree_oid(format)? {
        return Ok(false);
    }
    let mut seen = BTreeSet::new();
    collect_duplicate_leaf_paths(reader, format, tree_oid, Vec::new(), &mut seen)
}

fn collect_duplicate_leaf_paths(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    seen: &mut BTreeSet<Vec<u8>>,
) -> Result<bool> {
    let object = reader.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            tree_oid,
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            if collect_duplicate_leaf_paths(reader, format, &entry.oid, path, seen)? {
                return Ok(true);
            }
        } else if !seen.insert(path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_flat_tree(
    reader: &impl ObjectReader,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: Vec<u8>,
    entries: &mut MergeEntryMap,
) -> Result<()> {
    let object = reader.read_object(tree_oid)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitError::InvalidObject(format!(
            "expected tree {}, found {}",
            tree_oid,
            object.object_type.as_str()
        )));
    }
    for entry in TreeEntries::new(format, &object.body) {
        let entry = entry?;
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        if entry.mode == 0o040000 {
            collect_flat_tree(reader, format, &entry.oid, path, entries)?;
        } else {
            entries.insert(path, (entry.mode, entry.oid));
        }
    }
    Ok(())
}
