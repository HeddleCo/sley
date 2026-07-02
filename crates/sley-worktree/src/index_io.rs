//! Index<->worktree scanning internals: stat-cache reads, head-vs-index comparison, worktree-entry collection, and path/mode helpers.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::attributes::*;
use crate::checkout::*;
use crate::filter::*;
use crate::ignore::*;
use crate::index::*;
use crate::types_admin::*;

pub(crate) fn restore_index_entry(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entry: &IndexEntry,
    smudge_config: Option<&GitConfig>,
    stat_cache: Option<&IndexStatCache>,
) -> Result<Option<IndexEntry>> {
    restore_index_entry_maybe_delayed(
        worktree_root,
        git_dir,
        format,
        db,
        entry,
        smudge_config,
        stat_cache,
        None,
    )
}

pub(crate) fn restore_index_entry_maybe_delayed(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entry: &IndexEntry,
    smudge_config: Option<&GitConfig>,
    stat_cache: Option<&IndexStatCache>,
    mut delayed: Option<&mut DelayedCheckoutQueue>,
) -> Result<Option<IndexEntry>> {
    // A gitlink (mode 160000) names a commit in the submodule's repository, not
    // a blob here — reading it as a blob fails ("not found: blob object"). git's
    // `checkout_entry` S_IFGITLINK arm just ensures the submodule directory
    // exists and never touches an object; the submodule's content is `submodule
    // update` territory. Single gitlink rule via `sley_index::is_gitlink`.
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, entry.path.as_bytes())?;
        materialize_gitlink_dir(worktree_root, &dir_path)?;
        return Ok(None);
    }
    let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
    if let Some(stat_cache) = stat_cache {
        if let Ok(metadata) = fs::symlink_metadata(&file_path) {
            if stat_cache
                .reuse_index_entry_for_checkout(entry, &metadata)
                .is_some()
            {
                return Ok(None);
            }
        }
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let body: Cow<'_, [u8]> = match smudge_config {
        Some(config) => {
            let checks = smudge_attribute_checks_from_index(
                worktree_root,
                git_dir,
                format,
                entry.path.as_bytes(),
            )?;
            match apply_smudge_filter_with_attributes_maybe_delayed(
                config,
                &checks,
                entry.path.as_bytes(),
                &object.body,
                format,
                delayed.is_some() && (entry.mode & 0o170000) != 0o120000,
            )? {
                SmudgeFilterResult::Content(body) => body,
                SmudgeFilterResult::Delayed { process } => {
                    let queue = delayed
                        .as_deref_mut()
                        .expect("delay is only reported when a queue is available");
                    queue.enqueue(
                        process,
                        entry.path.as_bytes(),
                        &TrackedEntry {
                            mode: entry.mode,
                            oid: entry.oid,
                        },
                    );
                    return Ok(Some(unmaterialized_index_entry_from_index(entry)));
                }
            }
        }
        None => Cow::Borrowed(&object.body),
    };
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    write_blob_body_or_symlink(&file_path, entry.mode, &body, &object.body)?;
    let metadata = fs::symlink_metadata(&file_path)?;
    Ok(Some(index_entry_with_refreshed_stat(entry, &metadata)))
}

pub(crate) fn index_entry_with_refreshed_stat(
    entry: &IndexEntry,
    metadata: &fs::Metadata,
) -> IndexEntry {
    let mut refreshed = index_entry_from_metadata(entry.path.clone(), entry.oid, metadata);
    refreshed.mode = entry.mode;
    refreshed.flags = entry.flags;
    refreshed.flags_extended = entry.flags_extended;
    refreshed
}

pub(crate) fn restored_head_index_entry(
    _worktree_root: &Path,
    _db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    // This restores the index from a tree (reset --mixed / stash / sparse) WITHOUT
    // rewriting the worktree file, so the file on disk may hold different content
    // than `entry.oid`. Crucially we must NOT copy the worktree file's stat onto
    // this entry: that would make the cached stat match a file whose real content
    // hashes to a DIFFERENT oid, breaking git's "stat-match implies oid-match"
    // invariant that the status stat-cache relies on. Leave the whole stat tuple
    // zeroed, including size, so `reset --mixed --no-refresh` remains stat-dirty
    // until an explicit/default refresh validates it (t7102 cell 28).
    Ok(IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode: entry.mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: entry.oid,
        flags: path.len().min(0x0fff) as u16,
        flags_extended: 0,
        path: BString::from(path),
    })
}

pub(crate) fn index_entry_is_under_path(entry_path: &[u8], directory: &[u8]) -> bool {
    if directory.is_empty() {
        return true;
    }
    entry_path
        .strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix(b"/"))
        .is_some()
}

pub(crate) fn index_entry_from_metadata(
    path: impl Into<BString>,
    oid: ObjectId,
    metadata: &fs::Metadata,
) -> IndexEntry {
    let modified = metadata.modified().ok();
    let duration = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let mode = file_mode(metadata);
    let path = path.into();
    let flags = path.len().min(0x0fff) as u16;
    let mut entry = IndexEntry {
        ctime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        ctime_nanoseconds: duration.subsec_nanos(),
        mtime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        mtime_nanoseconds: duration.subsec_nanos(),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: index_size_from_metadata(metadata),
        oid,
        flags,
        flags_extended: 0,
        path,
    };
    apply_unix_metadata_to_index_entry(&mut entry, metadata);
    entry
}

pub(crate) fn index_entry_from_metadata_with_filemode(
    path: impl Into<BString>,
    oid: ObjectId,
    metadata: &fs::Metadata,
    trust_filemode: bool,
) -> IndexEntry {
    let mut entry = index_entry_from_metadata(path, oid, metadata);
    entry.mode = file_mode_with_trust(metadata, trust_filemode);
    entry
}

pub(crate) fn trust_executable_bit_from_git_dir(
    git_dir: &Path,
    config_parameters_env: Option<&str>,
) -> bool {
    sley_config::read_repo_config(git_dir, config_parameters_env)
        .ok()
        .as_ref()
        .map(trust_executable_bit)
        .unwrap_or(true)
}

pub(crate) fn trust_executable_bit(config: &GitConfig) -> bool {
    config.get_bool("core", None, "filemode").unwrap_or(true)
}

pub(crate) fn trust_symlinks_from_git_dir(
    git_dir: &Path,
    config_parameters_env: Option<&str>,
) -> bool {
    sley_config::read_repo_config(git_dir, config_parameters_env)
        .ok()
        .as_ref()
        .map(trust_symlinks)
        .unwrap_or(true)
}

pub(crate) fn trust_symlinks(config: &GitConfig) -> bool {
    config.get_bool("core", None, "symlinks").unwrap_or(true)
}

pub(crate) fn preferred_unmerged_mode_for_untrusted_worktree(
    entries: &[IndexEntry],
    trust_filemode: bool,
    trust_symlinks: bool,
) -> Option<u32> {
    if trust_filemode && trust_symlinks {
        return None;
    }
    let preferred = entries
        .iter()
        .find(|entry| entry.stage() == Stage::Ours)
        .or_else(|| entries.iter().find(|entry| entry.stage() == Stage::Base))?;
    if (!trust_symlinks && preferred.mode == 0o120000)
        || (!trust_filemode && matches!(preferred.mode, 0o100644 | 0o100755))
    {
        Some(preferred.mode)
    } else {
        None
    }
}

pub(crate) fn file_mode_with_trust(metadata: &fs::Metadata, trust_filemode: bool) -> u32 {
    if trust_filemode {
        file_mode(metadata)
    } else {
        0o100644
    }
}

#[cfg(unix)]
pub(crate) fn apply_unix_metadata_to_index_entry(entry: &mut IndexEntry, metadata: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt;

    entry.ctime_seconds = metadata.ctime().min(u32::MAX as i64).max(0) as u32;
    entry.ctime_nanoseconds = metadata.ctime_nsec().min(u32::MAX as i64).max(0) as u32;
    entry.dev = metadata.dev() as u32;
    entry.ino = metadata.ino() as u32;
    entry.uid = metadata.uid();
    entry.gid = metadata.gid();
}

#[cfg(not(unix))]
pub(crate) fn apply_unix_metadata_to_index_entry(
    _entry: &mut IndexEntry,
    _metadata: &fs::Metadata,
) {
}

pub(crate) fn index_size_from_metadata(metadata: &fs::Metadata) -> u32 {
    metadata.len().min(u32::MAX as u64) as u32
}

pub(crate) fn read_expected_object(
    db: &FileObjectDatabase,
    oid: &ObjectId,
    expected: ObjectType,
) -> Result<std::sync::Arc<EncodedObject>> {
    let object = db
        .read_object(oid)
        .map_err(|err| expect_missing_object_kind(err, *oid, missing_kind_for_type(expected)))?;
    if object.object_type != expected {
        return Err(GitError::InvalidObject(format!(
            "expected {} {}, found {}",
            expected.as_str(),
            oid,
            object.object_type.as_str()
        )));
    }
    Ok(object)
}

pub(crate) fn expect_missing_object_kind(
    err: GitError,
    oid: ObjectId,
    expected: MissingObjectKind,
) -> GitError {
    match err.not_found_kind() {
        Some(sley_core::NotFoundKind::Object { .. }) => GitError::object_kind_not_found_in(
            oid,
            expected,
            MissingObjectContext::WorktreeMaterialize,
        ),
        _ => err,
    }
}

pub(crate) fn missing_kind_for_type(object_type: ObjectType) -> MissingObjectKind {
    match object_type {
        ObjectType::Blob => MissingObjectKind::Blob,
        ObjectType::Tree => MissingObjectKind::Tree,
        ObjectType::Commit => MissingObjectKind::Commit,
        ObjectType::Tag => MissingObjectKind::Tag,
    }
}

pub(crate) fn read_commit(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    oid: &ObjectId,
) -> Result<Commit> {
    let object = read_expected_object(db, oid, ObjectType::Commit)?;
    Commit::parse(format, &object.body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedEntry {
    pub(crate) mode: u32,
    pub(crate) oid: ObjectId,
}

/// The short-status change byte for a pair of differing tracked entries: `T`
/// (typechange) when the file-type bits (`S_IFMT`) of the two modes differ —
/// regular↔symlink, regular↔gitlink — otherwise `M` (modified). git's diff
/// machinery sets `DIFF_STATUS_TYPE_CHANGED` for the former
/// (`diffcore.h`'s `DIFF_PAIR_TYPE_CHANGED`); `wt-status.c` renders it
/// `typechange:` / `T`. An exec-bit-only change (100644↔100755) stays `M`.
pub(crate) fn status_change_code(old_mode: u32, new_mode: u32) -> u8 {
    if sley_diff_merge::is_type_change(old_mode, new_mode) {
        b'T'
    } else {
        b'M'
    }
}

/// git's racy-git stat cache: the stage-0 index entries keyed by path (so the
/// worktree walk can reuse a cached oid when a file's stat shows it is unchanged
/// since it was staged) plus the index *file's* own mtime, which git uses as the
/// racy-clean reference timestamp.
///
/// SAFETY INVARIANT: trusting a cached oid by stat alone is only sound because
/// every code path that stamps a worktree stat onto an index entry also hashed
/// that exact file content (see `index_entry_from_metadata`), while tree-sourced
/// restores (reset --mixed / stash / sparse) leave the stat zeroed
/// (`restored_head_index_entry`). So a non-zero, non-racy stat match implies the
/// cached oid is the file's true content. When that does not hold we fall through
/// to a full read+filter+hash, so a modified file is never reported clean.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexStatCache {
    pub(crate) entries: HashMap<Vec<u8>, IndexEntry>,
    /// The index file's modification time as `(seconds, nanoseconds)`, or `None`
    /// when it could not be determined. Used as git's racy-clean reference.
    pub(crate) index_mtime: Option<(u64, u64)>,
}

impl IndexStatCache {
    /// Builds the cache from an already-parsed index plus the path of the index
    /// file on disk (whose mtime becomes the racy-clean reference). Only stage-0
    /// entries are retained; higher merge stages never describe a worktree file.
    pub(crate) fn from_index(index: &Index, index_path: &Path) -> Self {
        let index_mtime = fs::metadata(index_path)
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        Self::from_index_mtime(index, index_mtime)
    }

    pub(crate) fn from_index_mtime(index: &Index, index_mtime: Option<(u64, u64)>) -> Self {
        IndexStatCache {
            entries: stage0_index_entries(index),
            index_mtime,
        }
    }

    pub(crate) fn from_index_mtime_only(index_mtime: Option<(u64, u64)>) -> Self {
        IndexStatCache {
            entries: HashMap::new(),
            index_mtime,
        }
    }

    /// Whether `entry` is "racily clean" in git's sense: its cached mtime is not
    /// strictly older than the index file's mtime, so a same-timestamp write
    /// could have changed the content without moving the stat. Such entries must
    /// always be re-hashed.
    ///
    /// Conservative by construction: if the index mtime is unknown, or either
    /// side's mtime is zero (e.g. a tree-sourced entry whose stat was left
    /// zeroed), this returns `true` so the caller re-hashes rather than trusting
    /// a stat we cannot prove safe.
    fn is_racily_clean(&self, entry: &IndexEntry) -> bool {
        let Some(index_mtime) = self.index_mtime else {
            return true;
        };
        if index_mtime == (0, 0) {
            return true;
        }
        let entry_mtime = (
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        );
        if entry_mtime == (0, 0) {
            return true;
        }
        // Racy unless the index was written strictly after the entry's mtime.
        index_mtime <= entry_mtime
    }

    fn is_racily_clean_ref(&self, entry: &IndexEntryRef<'_>) -> bool {
        let Some(index_mtime) = self.index_mtime else {
            return true;
        };
        if index_mtime == (0, 0) {
            return true;
        }
        let entry_mtime = (
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        );
        if entry_mtime == (0, 0) {
            return true;
        }
        index_mtime <= entry_mtime
    }

    /// Whether the index has a stage-0 entry for `git_path` (i.e. the path is
    /// tracked). Used to skip hashing untracked worktree files.
    fn contains(&self, git_path: &[u8]) -> bool {
        self.entries.contains_key(git_path)
    }

    fn tracked_entry(&self, git_path: &[u8]) -> Option<TrackedEntry> {
        self.entries.get(git_path).map(|entry| TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        })
    }

    pub(crate) fn index_entry(&self, git_path: &[u8]) -> Option<&IndexEntry> {
        self.entries.get(git_path)
    }

    /// Returns the cached [`TrackedEntry`] for `git_path` (reusing its stored
    /// oid, so the caller can SKIP reading, filtering, and hashing the file) only
    /// when the worktree file is provably unchanged since it was staged: a
    /// stage-0 entry exists, its recorded mode matches the file's current mode
    /// (catching pure `chmod`s that do not move mtime), the size+mtime stat
    /// check passes, and the entry is not racily clean. Otherwise returns `None`
    /// and the caller hashes the file as usual.
    pub(crate) fn reuse_tracked_entry(
        &self,
        git_path: &[u8],
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        let entry = self.entries.get(git_path)?;
        self.reuse_index_entry(entry, worktree_metadata)
    }

    pub(crate) fn reuse_index_entry(
        &self,
        entry: &IndexEntry,
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        // Gitlink: reusable as-is whenever the worktree path is a directory (a
        // submodule is never re-hashed; its cached stat is ignored). Routes
        // through the single `sley_index::gitlink_stat_verdict` rule so the
        // gitlink-vs-040000 mode mismatch never spuriously rejects it.
        if sley_index::is_gitlink(entry.mode) {
            return match sley_index::gitlink_stat_verdict(worktree_metadata) {
                sley_index::GitlinkStatVerdict::Populated => Some(TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                }),
                sley_index::GitlinkStatVerdict::TypeChanged => None,
            };
        }
        if entry.mode != worktree_entry_mode(worktree_metadata) {
            return None;
        }
        if !worktree_entry_is_uptodate(entry, worktree_metadata) {
            return None;
        }
        if self.is_racily_clean(entry) {
            return None;
        }
        Some(TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        })
    }

    fn reuse_index_entry_for_checkout(
        &self,
        entry: &IndexEntry,
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        if let Some(tracked) = self.reuse_index_entry(entry, worktree_metadata) {
            return Some(tracked);
        }
        if u64::from(entry.size) != 0 || worktree_metadata.len() == 0 {
            return None;
        }
        if entry.mode != worktree_entry_mode(worktree_metadata) {
            return None;
        }
        let (mtime_seconds, mtime_nanoseconds) = file_mtime_parts(worktree_metadata)?;
        if u64::from(entry.mtime_seconds) != mtime_seconds
            || u64::from(entry.mtime_nanoseconds) != mtime_nanoseconds
        {
            return None;
        }
        if self.is_racily_clean(entry) {
            return None;
        }
        Some(TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        })
    }

    pub(crate) fn reuse_index_entry_ref(
        &self,
        entry: &IndexEntryRef<'_>,
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        if sley_index::is_gitlink(entry.mode) {
            return match sley_index::gitlink_stat_verdict(worktree_metadata) {
                sley_index::GitlinkStatVerdict::Populated => Some(TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                }),
                sley_index::GitlinkStatVerdict::TypeChanged => None,
            };
        }
        if entry.mode != worktree_entry_mode(worktree_metadata) {
            return None;
        }
        if !worktree_entry_ref_is_uptodate(entry, worktree_metadata) {
            return None;
        }
        if self.is_racily_clean_ref(entry) {
            return None;
        }
        Some(TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        })
    }

    /// The stage-0 gitlink (mode 160000) index entry at `git_path`, if any.
    fn gitlink_entry(&self, git_path: &[u8]) -> Option<&IndexEntry> {
        self.entries
            .get(git_path)
            .filter(|entry| sley_index::is_gitlink(entry.mode))
    }
}

pub(crate) fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    Ok(read_index_entries_with_stat_cache(git_dir, format, &db)?.0)
}

pub(crate) fn read_all_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeSet<Vec<u8>>> {
    let index_path = repository_index_path(git_dir);
    let bytes = match fs::read(index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(err.into()),
    };
    let index = Index::parse(&bytes, format)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| entry.path.into_bytes())
        .collect())
}

pub(crate) fn resolve_head_tree_oid(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<Option<ObjectId>> {
    let Some(commit_oid) = resolve_head_commit_oid(git_dir, format)? else {
        return Ok(None);
    };
    if let Some(tree_oid) = sley_rev::commit_graph_tree_oid(git_dir, format, &commit_oid)? {
        return Ok(Some(tree_oid));
    }
    let object = read_expected_object(db, &commit_oid, ObjectType::Commit)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    Ok(Some(commit.tree))
}

pub(crate) fn resolve_head_commit_oid(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<Option<ObjectId>> {
    let refs = FileRefStore::new(git_dir, format);
    sley_refs::resolve_ref_peeled(&refs, "HEAD")
}

pub(crate) fn status_row_is_untracked_or_ignored(entry: ShortStatusRow<'_>) -> bool {
    matches!((entry.index, entry.worktree), (b'?', b'?') | (b'!', b'!'))
}

pub(crate) fn checkout_switch_head_symbolic(
    refs: &FileRefStore,
    branch_ref: String,
    committer: Vec<u8>,
    branch: &str,
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
) -> Result<()> {
    // Reflog "from" side: the previous branch's short name, or the commit id
    // when HEAD was detached (git's `checkout: moving from X to Y` shape,
    // which `@{-N}` resolution parses).
    let from = match refs.read_ref("HEAD") {
        Ok(Some(RefTarget::Symbolic(name))) => name
            .strip_prefix("refs/heads/")
            .unwrap_or(&name)
            .to_string(),
        Ok(Some(RefTarget::Direct(oid))) => oid.to_hex(),
        _ => "HEAD".to_string(),
    };
    let mut tx = refs.transaction();
    let reflog = match (old_oid, new_oid) {
        (Some(old_oid), Some(new_oid)) => Some(ReflogEntry {
            old_oid,
            new_oid,
            committer,
            message: format!("checkout: moving from {from} to {branch}").into_bytes(),
        }),
        _ => None,
    };
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Symbolic(branch_ref),
        reflog,
    });
    tx.commit()
}

pub(crate) fn head_matches_index_from_entries(
    index: &Index,
    format: ObjectFormat,
    head_tree_oid: &ObjectId,
) -> Result<bool> {
    if index
        .entries
        .iter()
        .any(|entry| entry.stage() != Stage::Normal)
    {
        return Ok(false);
    }
    let index_tree = index_entry_tree_oid_without_cache(index, format)?;
    Ok(&index_tree == head_tree_oid)
}

pub(crate) fn head_matches_borrowed_index_from_entries(
    index: &BorrowedIndex<'_>,
    format: ObjectFormat,
    head_tree_oid: &ObjectId,
) -> Result<bool> {
    if index
        .entries
        .iter()
        .any(|entry| entry.stage() != Stage::Normal)
    {
        return Ok(false);
    }
    let index_tree = borrowed_index_entry_tree_oid_without_cache(index, format)?;
    Ok(&index_tree == head_tree_oid)
}

/// Parses the index a single time and returns both the path -> [`TrackedEntry`]
/// map used for status comparisons AND the [`IndexStatCache`] used to short-cut
/// the worktree walk, avoiding a second parse of the same file.
pub(crate) fn read_index_entries_with_stat_cache(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<(BTreeMap<Vec<u8>, TrackedEntry>, IndexStatCache, bool)> {
    let (index, stat_cache, head_matches_index) = read_index_with_stat_cache(git_dir, format, db)?;
    let tracked = index_entries_from_index(index);
    Ok((tracked, stat_cache, head_matches_index))
}

pub(crate) fn index_entries_from_index(index: Index) -> BTreeMap<Vec<u8>, TrackedEntry> {
    index
        .entries
        .into_iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .map(|entry| {
            (
                entry.path.into_bytes(),
                TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                },
            )
        })
        .collect()
}

pub(crate) fn read_index_with_stat_cache(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<(Index, IndexStatCache, bool)> {
    read_index_with_stat_cache_entries(git_dir, format, db, true)
}

pub(crate) fn read_index_with_stat_cache_entries(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    include_entries: bool,
) -> Result<(Index, IndexStatCache, bool)> {
    let index_path = repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Index {
                    version: 2,
                    entries: Vec::new(),
                    extensions: Vec::new(),
                    checksum: None,
                },
                IndexStatCache::default(),
                false,
            ));
        }
        Err(err) => return Err(err.into()),
    };
    let index = sley_index::read_repository_index(git_dir, format)?;
    let index_mtime = file_mtime_parts(&index_metadata);
    let stat_cache = if include_entries {
        IndexStatCache::from_index_mtime(&index, index_mtime)
    } else {
        IndexStatCache::from_index_mtime_only(index_mtime)
    };
    let head_matches_index = match resolve_head_tree_oid(git_dir, format, db)? {
        Some(head_tree_oid) => head_matches_index_from_entries(&index, format, &head_tree_oid)?,
        None => false,
    };
    Ok((index, stat_cache, head_matches_index))
}

pub(crate) fn head_tree_entries(
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
    let object = read_expected_object(db, &commit_oid, ObjectType::Commit)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, &mut entries)?;
    Ok(entries)
}

pub(crate) fn tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, tree_oid, &mut entries)?;
    Ok(entries)
}

/// Flatten a tree's blob leaves into `entries`, keyed by full path.
///
/// Delegates to the canonical [`sley_diff_merge::flatten_tree`] (the local
/// recursive flattener was a byte-identical copy) and adapts its
/// `(mode, oid)` tuples into this module's [`TrackedEntry`]. Entries already
/// present in `entries` are overwritten, matching the previous insert-based
/// behaviour.
pub(crate) fn collect_tree_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    entries: &mut BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    for (path, (mode, oid)) in sley_diff_merge::flatten_tree(db, format, tree_oid)? {
        entries.insert(path, TrackedEntry { mode, oid });
    }
    Ok(())
}

/// Like a full worktree walk, but accepts the index's [`IndexStatCache`] so the
/// walk can reuse a cached oid for files that are provably unchanged since they
/// were staged, skipping the read+filter+hash for those paths. Passing `None`
/// hashes every file when no stat cache is supplied.
pub(crate) fn worktree_entries_with_stat_cache(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stat_cache: Option<&IndexStatCache>,
    tracked_paths: Option<&BTreeSet<Vec<u8>>>,
    ignores: Option<&mut IgnoreMatcher>,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    Ok(worktree_entries_with_submodule_dirt(
        worktree_root,
        git_dir,
        format,
        stat_cache,
        tracked_paths,
        ignores,
    )?
    .0)
}

/// Tracked worktree entries keyed by repo path, plus the dirt mask
/// ([`DIRTY_SUBMODULE_MODIFIED`] / [`DIRTY_SUBMODULE_UNTRACKED`]) for every
/// tracked gitlink path whose submodule working tree is dirty.
pub(crate) type WorktreeEntriesWithDirt = (BTreeMap<Vec<u8>, TrackedEntry>, BTreeMap<Vec<u8>, u8>);

/// Status worktree snapshot: tracked/untracked entries, gitlink dirt masks, and
/// tracked paths observed in the worktree.
pub(crate) type StatusWorktreeSnapshot = (
    BTreeMap<Vec<u8>, TrackedEntry>,
    BTreeMap<Vec<u8>, u8>,
    HashSet<Vec<u8>>,
);

/// Like [`worktree_entries_with_stat_cache`], but also reports, for every
/// tracked gitlink path whose submodule working tree is dirty, the dirt mask
/// ([`DIRTY_SUBMODULE_MODIFIED`] / [`DIRTY_SUBMODULE_UNTRACKED`]).
pub(crate) fn worktree_entries_with_submodule_dirt(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stat_cache: Option<&IndexStatCache>,
    tracked_paths: Option<&BTreeSet<Vec<u8>>>,
    ignores: Option<&mut IgnoreMatcher>,
) -> Result<WorktreeEntriesWithDirt> {
    let mut entries = BTreeMap::new();
    let mut submodule_dirt_map = BTreeMap::new();
    let mut tracked_presence = HashSet::new();
    // Worktree blobs are compared to the index by OID, so they must be passed
    // through the clean filter (core.autocrlf / .gitattributes) first -- exactly
    // as `git add` would store them. With no filter configured this is an exact
    // passthrough, so unfiltered repositories see identical OIDs.
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    // Seed the matcher with the repo-wide sources only; each directory's
    // `.gitattributes` is folded in by `collect_worktree_entries` as it descends,
    // so the worktree is read exactly once (a separate full-tree attribute pass was
    // a second traversal of every directory).
    let mut attr_matcher = AttributeMatcher::from_worktree_base(worktree_root);
    let attr_requested = filter_attribute_names();
    let mut context = WorktreeEntriesWalk {
        git_dir,
        format,
        config: &config,
        matcher: &mut attr_matcher,
        requested: &attr_requested,
        stat_cache,
        known_tracked_paths: tracked_paths,
        tracked_paths,
        ignores,
        entries: &mut entries,
        submodule_dirt: &mut submodule_dirt_map,
        tracked_presence: &mut tracked_presence,
        record_clean_tracked: true,
    };
    collect_worktree_entries(&mut context, worktree_root, &[])?;
    Ok((entries, submodule_dirt_map))
}

pub(crate) fn status_worktree_entries_with_submodule_dirt(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stat_cache: &IndexStatCache,
    known_tracked_paths: Option<&BTreeSet<Vec<u8>>>,
    tracked_paths: Option<&BTreeSet<Vec<u8>>>,
    ignores: Option<&mut IgnoreMatcher>,
) -> Result<StatusWorktreeSnapshot> {
    let mut entries = BTreeMap::new();
    let mut submodule_dirt_map = BTreeMap::new();
    let mut tracked_presence = HashSet::new();
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut attr_matcher = AttributeMatcher::from_worktree_base(worktree_root);
    let attr_requested = filter_attribute_names();
    let mut context = WorktreeEntriesWalk {
        git_dir,
        format,
        config: &config,
        matcher: &mut attr_matcher,
        requested: &attr_requested,
        stat_cache: Some(stat_cache),
        known_tracked_paths,
        tracked_paths,
        ignores,
        entries: &mut entries,
        submodule_dirt: &mut submodule_dirt_map,
        tracked_presence: &mut tracked_presence,
        record_clean_tracked: false,
    };
    collect_worktree_entries(&mut context, worktree_root, &[])?;
    Ok((entries, submodule_dirt_map, tracked_presence))
}

pub(crate) fn worktree_entry_for_git_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    git_path: &[u8],
    expected_oid: &ObjectId,
    expected_mode: u32,
    stat_cache: Option<&IndexStatCache>,
) -> Result<Option<TrackedEntry>> {
    if git_path_has_symlink_parent(worktree_root, git_path)? {
        return Ok(None);
    }
    let absolute = worktree_root.join(repo_path_to_os_path(git_path)?);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };

    if sley_index::is_gitlink(expected_mode) {
        if !metadata.is_dir() {
            return Ok(Some(TrackedEntry {
                mode: worktree_entry_mode(&metadata),
                oid: ObjectId::null(format),
            }));
        }
        let oid = sley_diff_merge::gitlink_head_oid(&absolute, format).unwrap_or(*expected_oid);
        return Ok(Some(TrackedEntry {
            mode: sley_index::GITLINK_MODE,
            oid,
        }));
    }

    if metadata.is_dir() {
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if !(metadata.is_file() || metadata.file_type().is_symlink()) {
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if let Some(tracked) =
        stat_cache.and_then(|cache| cache.reuse_tracked_entry(git_path, &metadata))
    {
        return Ok(Some(tracked));
    }

    let mode = worktree_entry_mode(&metadata);
    let body = if metadata.file_type().is_symlink() {
        symlink_target_bytes(&absolute)?
    } else {
        let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
        let body = fs::read(&absolute)?;
        let clean = apply_clean_filter(worktree_root, git_dir, &config, git_path, &body)?;
        let oid = match stat_cache.and_then(|cache| cache.index_entry(git_path)) {
            Some(index_entry) => clean_filtered_oid_for_status(
                format,
                &body,
                clean,
                index_entry.oid,
                index_entry.size,
                &metadata,
            )?,
            None => EncodedObject::new(ObjectType::Blob, clean).object_id(format)?,
        };
        return Ok(Some(TrackedEntry { mode, oid }));
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

pub(crate) fn worktree_entry_for_index_entry_with_attributes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entry: &IndexEntry,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
) -> Result<Option<TrackedEntry>> {
    let git_path = index_entry.path.as_bytes();
    let expected_mode = index_entry.mode;
    if git_path_has_symlink_parent(worktree_root, git_path)? {
        return Ok(None);
    }
    let absolute = worktree_root.join(repo_path_to_os_path(git_path)?);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();

    if sley_index::is_gitlink(expected_mode) {
        if !file_type.is_dir() {
            return Ok(Some(TrackedEntry {
                mode: worktree_entry_mode(&metadata),
                oid: ObjectId::null(format),
            }));
        }
        let oid = sley_diff_merge::gitlink_head_oid(&absolute, format).unwrap_or(index_entry.oid);
        return Ok(Some(TrackedEntry {
            mode: sley_index::GITLINK_MODE,
            oid,
        }));
    }

    if file_type.is_dir() {
        if expected_mode != 0o040000 {
            return Ok(None);
        }
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if let Some(tracked) = stat_cache.reuse_index_entry(index_entry, &metadata) {
        return Ok(Some(tracked));
    }

    let mode = worktree_entry_mode(&metadata);
    let body = if file_type.is_symlink() {
        symlink_target_bytes(&absolute)?
    } else {
        let body = fs::read(&absolute)?;
        let clean_filter = tracked_only_clean_filter(clean_filter, worktree_root, git_dir);
        clean_filter.read_attributes_for_path(worktree_root, git_path)?;
        let checks =
            clean_filter
                .matcher
                .attributes_for_path(git_path, &clean_filter.requested, false);
        let clean =
            apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?;
        let oid = clean_filtered_oid_for_status(
            format,
            &body,
            clean,
            index_entry.oid,
            index_entry.size,
            &metadata,
        )?;
        return Ok(Some(TrackedEntry { mode, oid }));
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

pub(crate) fn worktree_entry_for_index_entry_ref_with_attributes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entry: &IndexEntryRef<'_>,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
) -> Result<Option<TrackedEntry>> {
    let git_path = index_entry.path;
    let expected_mode = index_entry.mode;
    if git_path_has_symlink_parent(worktree_root, git_path)? {
        return Ok(None);
    }
    let absolute = worktree_root.join(repo_path_to_os_path(git_path)?);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();

    if sley_index::is_gitlink(expected_mode) {
        if !file_type.is_dir() {
            return Ok(Some(TrackedEntry {
                mode: worktree_entry_mode(&metadata),
                oid: ObjectId::null(format),
            }));
        }
        let oid = sley_diff_merge::gitlink_head_oid(&absolute, format).unwrap_or(index_entry.oid);
        return Ok(Some(TrackedEntry {
            mode: sley_index::GITLINK_MODE,
            oid,
        }));
    }

    if file_type.is_dir() {
        if expected_mode != 0o040000 {
            return Ok(None);
        }
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(Some(TrackedEntry {
            mode: worktree_entry_mode(&metadata),
            oid: ObjectId::null(format),
        }));
    }

    if let Some(tracked) = stat_cache.reuse_index_entry_ref(index_entry, &metadata) {
        return Ok(Some(tracked));
    }

    let mode = worktree_entry_mode(&metadata);
    let body = if file_type.is_symlink() {
        symlink_target_bytes(&absolute)?
    } else {
        let body = fs::read(&absolute)?;
        let clean_filter = tracked_only_clean_filter(clean_filter, worktree_root, git_dir);
        clean_filter.read_attributes_for_path(worktree_root, git_path)?;
        let checks =
            clean_filter
                .matcher
                .attributes_for_path(git_path, &clean_filter.requested, false);
        let clean =
            apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?;
        let oid = clean_filtered_oid_for_status(
            format,
            &body,
            clean,
            index_entry.oid,
            index_entry.size,
            &metadata,
        )?;
        return Ok(Some(TrackedEntry { mode, oid }));
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

pub(crate) fn clean_filtered_oid_for_status(
    format: ObjectFormat,
    raw_body: &[u8],
    clean_body: Vec<u8>,
    index_oid: ObjectId,
    index_size: u32,
    metadata: &fs::Metadata,
) -> Result<ObjectId> {
    let clean_oid = EncodedObject::new(ObjectType::Blob, clean_body).object_id(format)?;
    let metadata_size = index_size_from_metadata(metadata);
    if clean_oid == index_oid && index_size != 0 && index_size != metadata_size {
        return EncodedObject::new(ObjectType::Blob, raw_body.to_vec()).object_id(format);
    }
    Ok(clean_oid)
}

#[derive(Default)]
pub struct StatCleanFilterValidator {
    clean_filter: Option<TrackedOnlyCleanFilter>,
}

impl StatCleanFilterValidator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn validate_path(
        &mut self,
        worktree_root: &Path,
        git_dir: &Path,
        format: ObjectFormat,
        _index_mode: u32,
        index_oid: ObjectId,
        _index_size: u32,
        git_path: &[u8],
        absolute_path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<Option<sley_diff_merge::IndexWorktreeValidatedEntry>> {
        if !metadata.is_file() {
            return Ok(None);
        }
        let clean_filter =
            tracked_only_clean_filter(&mut self.clean_filter, worktree_root, git_dir);
        clean_filter.read_attributes_for_path(worktree_root, git_path)?;
        let checks =
            clean_filter
                .matcher
                .attributes_for_path(git_path, &clean_filter.requested, false);
        let body = fs::read(absolute_path)?;
        if !clean_encoding_needs_stat_match_validation(&clean_filter.config, &checks) {
            let clean =
                apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?;
            let oid = EncodedObject::new(ObjectType::Blob, clean).object_id(format)?;
            let oid = clean_or_raw_oid_for_index(format, &body, oid, index_oid)?;
            return Ok(Some(sley_diff_merge::IndexWorktreeValidatedEntry {
                mode: worktree_entry_mode(metadata),
                oid,
            }));
        }
        validate_clean_encoding_for_stat_match(&clean_filter.config, &checks, git_path, &body)?;
        let clean =
            apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?;
        let oid = EncodedObject::new(ObjectType::Blob, clean).object_id(format)?;
        let oid = clean_or_raw_oid_for_index(format, &body, oid, index_oid)?;
        Ok(Some(sley_diff_merge::IndexWorktreeValidatedEntry {
            mode: worktree_entry_mode(metadata),
            oid,
        }))
    }
}

fn clean_or_raw_oid_for_index(
    format: ObjectFormat,
    raw_body: &[u8],
    clean_oid: ObjectId,
    index_oid: ObjectId,
) -> Result<ObjectId> {
    if clean_oid == index_oid {
        return Ok(clean_oid);
    }
    let raw_oid = EncodedObject::new(ObjectType::Blob, raw_body.to_vec()).object_id(format)?;
    if raw_oid == index_oid {
        Ok(raw_oid)
    } else {
        Ok(clean_oid)
    }
}

pub(crate) struct TrackedOnlyCleanFilter {
    pub(crate) config: GitConfig,
    pub(crate) matcher: AttributeMatcher,
    pub(crate) requested: Vec<Vec<u8>>,
    pub(crate) attribute_dirs: BTreeSet<Vec<u8>>,
}

impl TrackedOnlyCleanFilter {
    pub(crate) fn read_attributes_for_path(
        &mut self,
        worktree_root: &Path,
        git_path: &[u8],
    ) -> Result<()> {
        self.read_attribute_dir(worktree_root, &[])?;
        let mut prefix = Vec::new();
        let mut parts = git_path.split(|byte| *byte == b'/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(part);
            self.read_attribute_dir(worktree_root, &prefix)?;
        }
        Ok(())
    }

    fn read_attribute_dir(&mut self, worktree_root: &Path, git_path: &[u8]) -> Result<()> {
        if !self.attribute_dirs.insert(git_path.to_vec()) {
            return Ok(());
        }
        let dir = if git_path.is_empty() {
            worktree_root.to_path_buf()
        } else {
            worktree_root.join(repo_path_to_os_path(git_path)?)
        };
        read_dir_attribute_patterns(worktree_root, &dir, &mut self.matcher)
    }
}

pub(crate) fn tracked_only_clean_filter<'a>(
    clean_filter: &'a mut Option<TrackedOnlyCleanFilter>,
    worktree_root: &Path,
    git_dir: &Path,
) -> &'a mut TrackedOnlyCleanFilter {
    if clean_filter.is_none() {
        *clean_filter = Some(TrackedOnlyCleanFilter {
            config: sley_config::read_repo_config(git_dir, None).unwrap_or_default(),
            matcher: AttributeMatcher::from_worktree_base(worktree_root),
            requested: filter_attribute_names(),
            attribute_dirs: BTreeSet::new(),
        });
    }
    clean_filter
        .as_mut()
        .expect("tracked-only clean filter initialized")
}

pub(crate) fn tracked_only_clean_filter_with_config<'a>(
    clean_filter: &'a mut Option<TrackedOnlyCleanFilter>,
    worktree_root: &Path,
    config: &GitConfig,
) -> &'a mut TrackedOnlyCleanFilter {
    if clean_filter.is_none() {
        *clean_filter = Some(TrackedOnlyCleanFilter {
            config: config.clone(),
            matcher: AttributeMatcher::from_worktree_base(worktree_root),
            requested: filter_attribute_names(),
            attribute_dirs: BTreeSet::new(),
        });
    }
    clean_filter
        .as_mut()
        .expect("tracked-only clean filter initialized")
}

pub(crate) struct WorktreeEntriesWalk<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    config: &'a GitConfig,
    matcher: &'a mut AttributeMatcher,
    requested: &'a [Vec<u8>],
    stat_cache: Option<&'a IndexStatCache>,
    known_tracked_paths: Option<&'a BTreeSet<Vec<u8>>>,
    tracked_paths: Option<&'a BTreeSet<Vec<u8>>>,
    ignores: Option<&'a mut IgnoreMatcher>,
    entries: &'a mut BTreeMap<Vec<u8>, TrackedEntry>,
    /// Dirt masks for tracked gitlink paths whose submodule worktree is dirty.
    submodule_dirt: &'a mut BTreeMap<Vec<u8>, u8>,
    tracked_presence: &'a mut HashSet<Vec<u8>>,
    record_clean_tracked: bool,
}

impl WorktreeEntriesWalk<'_> {
    fn mark_tracked_present(&mut self, git_path: &[u8]) {
        self.tracked_presence.insert(git_path.to_vec());
    }

    fn tracked_entry_for(&self, git_path: &[u8]) -> Option<TrackedEntry> {
        self.stat_cache
            .and_then(|cache| cache.tracked_entry(git_path))
    }

    fn should_record_tracked_entry(&self, git_path: &[u8], entry: &TrackedEntry) -> bool {
        self.record_clean_tracked
            || self.is_intent_to_add(git_path)
            || self
                .tracked_entry_for(git_path)
                .is_none_or(|tracked| tracked != *entry)
    }

    /// An intent-to-add (`git add -N`) entry must always be recorded into the
    /// worktree snapshot: even when its bytes match the empty-blob placeholder,
    /// status reports it as worktree-added (`.A`), so it can never be folded
    /// away as a clean match.
    fn is_intent_to_add(&self, git_path: &[u8]) -> bool {
        self.stat_cache
            .and_then(|cache| cache.index_entry(git_path))
            .is_some_and(IndexEntry::is_intent_to_add)
    }
}

pub(crate) fn git_path_append_component(parent: &[u8], component: &std::ffi::OsStr) -> Vec<u8> {
    let component = os_str_component_bytes(component);
    let separator = usize::from(!parent.is_empty());
    let mut path = Vec::with_capacity(parent.len() + separator + component.len());
    if !parent.is_empty() {
        path.extend_from_slice(parent);
        path.push(b'/');
    }
    path.extend_from_slice(component.as_ref());
    path
}

pub(crate) fn git_path_push_component(path: &mut Vec<u8>, component: &std::ffi::OsStr) -> usize {
    let original_len = path.len();
    let component = os_str_component_bytes(component);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(component.as_ref());
    original_len
}

#[cfg(unix)]
pub(crate) fn os_str_component_bytes(component: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;

    Cow::Borrowed(component.as_bytes())
}

#[cfg(not(unix))]
pub(crate) fn os_str_component_bytes(component: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    Cow::Owned(component.to_string_lossy().into_owned().into_bytes())
}

pub(crate) fn collect_worktree_entries(
    context: &mut WorktreeEntriesWalk<'_>,
    dir: &Path,
    dir_git_path: &[u8],
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    // Fold this directory's `.gitattributes` into the matcher before processing its
    // files, so lookups for files here (and below) see it. This is what lets the
    // walk read the tree once instead of doing a separate full-tree attribute pass.
    read_dir_attribute_patterns_for_base(dir, dir_git_path, context.matcher)?;
    if let Some(ignores) = context.ignores.as_deref_mut() {
        read_dir_ignore_patterns_for_base(dir, dir_git_path, ignores)?;
    }
    let mut dir_entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    dir_entries.sort_by_key(|entry| entry.file_name());
    for entry in dir_entries {
        let file_name = entry.file_name();
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_same_path(&path, context.git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let git_path = git_path_append_component(dir_git_path, &file_name);
        if context
            .ignores
            .as_ref()
            .is_some_and(|ignores| ignores.is_ignored(&git_path, metadata.is_dir()))
        {
            let tracked = context.known_tracked_paths.is_some_and(|tracked_paths| {
                if metadata.is_dir() {
                    tracked_paths_may_contain(tracked_paths, &git_path)
                } else {
                    tracked_paths.contains(&git_path)
                }
            });
            if !tracked {
                continue;
            }
            if metadata.is_dir() {
                collect_worktree_entries(context, &path, &git_path)?;
                continue;
            }
        }
        if metadata.is_dir() {
            // A directory staged as a gitlink (mode 160000) is opaque: the walk
            // never descends into it. Its worktree "content" is the commit the
            // embedded repository has checked out (upstream ce_compare_gitlink):
            // a populated submodule reports its HEAD (plus a dirt mask when its
            // own tree has modified/untracked content); an unpopulated
            // directory — no repository, or no commit checked out — always
            // matches the staged oid.
            if let Some(index_entry) = context
                .stat_cache
                .and_then(|cache| cache.gitlink_entry(&git_path))
            {
                context.mark_tracked_present(&git_path);
                let oid = sley_diff_merge::gitlink_head_oid(&path, context.format)
                    .unwrap_or(index_entry.oid);
                let dirt = submodule_dirt_checked(&path)?;
                if dirt != 0 {
                    context.submodule_dirt.insert(git_path.clone(), dirt);
                }
                let tracked = TrackedEntry {
                    mode: sley_index::GITLINK_MODE,
                    oid,
                };
                if dirt != 0 || context.should_record_tracked_entry(&git_path, &tracked) {
                    context.entries.insert(git_path, tracked);
                }
                continue;
            }
            if is_nested_repository_boundary(&path, context.git_dir) {
                if let Some(tracked_paths) = context.tracked_paths
                    && !tracked_paths_may_contain(tracked_paths, &git_path)
                {
                    continue;
                }
                context.entries.insert(
                    git_path,
                    TrackedEntry {
                        mode: 0o040000,
                        oid: ObjectId::null(context.format),
                    },
                );
                continue;
            }
            if let Some(tracked_paths) = context.tracked_paths
                && !tracked_paths_may_contain(tracked_paths, &git_path)
            {
                continue;
            }
            collect_worktree_entries(context, &path, &git_path)?;
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            if let Some(tracked_paths) = context.tracked_paths
                && !tracked_paths.contains(&git_path)
            {
                continue;
            }
            let entry_mode = worktree_entry_mode(&metadata);
            // git's racy-git stat shortcut: when the index's cached stat proves
            // this file is unchanged since it was staged, reuse the staged oid
            // and skip the read+filter+hash entirely. `reuse_tracked_entry`
            // returns `Some` ONLY for a non-racy size+mtime+mode match, so a
            // modified file always falls through to the full hash below and is
            // never silently reported clean.
            if let Some(tracked) = context
                .stat_cache
                .and_then(|cache| cache.reuse_tracked_entry(&git_path, &metadata))
            {
                context.mark_tracked_present(&git_path);
                if context.record_clean_tracked || context.is_intent_to_add(&git_path) {
                    context.entries.insert(git_path, tracked);
                }
                continue;
            }
            // A file absent from the index is untracked: status and the
            // index-vs-worktree diff report it by *presence* (`??` / nothing), never
            // by content, so computing its oid is wasted work — git never hashes
            // untracked files. Record presence with a null oid and skip the
            // read+filter+hash. Without a stat cache we cannot tell tracked from
            // untracked, so fall through and hash as before.
            if context
                .stat_cache
                .is_some_and(|cache| !cache.contains(&git_path))
            {
                context.entries.insert(
                    git_path,
                    TrackedEntry {
                        mode: entry_mode,
                        oid: ObjectId::null(context.format),
                    },
                );
                continue;
            }
            let body = if metadata.file_type().is_symlink() {
                // The blob for a symlink is the raw link target; clean filters
                // never apply because git treats symlink content as opaque.
                symlink_target_bytes(&path)?
            } else {
                let body = fs::read(&path)?;
                // Resolve this path's attributes against the prebuilt matcher (a cheap
                // pattern match) and apply the clean filter -- no per-file matcher
                // rebuild. With no attributes/autocrlf configured this is an exact
                // passthrough, so the stored OID is unchanged.
                let checks =
                    context
                        .matcher
                        .attributes_for_path(&git_path, context.requested, false);
                let clean =
                    apply_clean_filter_with_attributes(context.config, &checks, &git_path, &body)?;
                let oid = match context
                    .stat_cache
                    .and_then(|cache| cache.index_entry(&git_path))
                {
                    Some(index_entry) => clean_filtered_oid_for_status(
                        context.format,
                        &body,
                        clean,
                        index_entry.oid,
                        index_entry.size,
                        &metadata,
                    )?,
                    None => {
                        EncodedObject::new(ObjectType::Blob, clean).object_id(context.format)?
                    }
                };
                let tracked = TrackedEntry {
                    mode: entry_mode,
                    oid,
                };
                if context
                    .stat_cache
                    .is_some_and(|cache| cache.contains(&git_path))
                {
                    context.mark_tracked_present(&git_path);
                    if context.should_record_tracked_entry(&git_path, &tracked) {
                        context.entries.insert(git_path, tracked);
                    }
                } else {
                    context.entries.insert(git_path, tracked);
                }
                continue;
            };
            let oid = EncodedObject::new(ObjectType::Blob, body).object_id(context.format)?;
            let tracked = TrackedEntry {
                mode: entry_mode,
                oid,
            };
            if context
                .stat_cache
                .is_some_and(|cache| cache.contains(&git_path))
            {
                context.mark_tracked_present(&git_path);
                if context.should_record_tracked_entry(&git_path, &tracked) {
                    context.entries.insert(git_path, tracked);
                }
            } else {
                context.entries.insert(git_path, tracked);
            }
        }
    }
    Ok(())
}

pub(crate) fn tracked_paths_may_contain(
    tracked_paths: &BTreeSet<Vec<u8>>,
    directory: &[u8],
) -> bool {
    if tracked_paths.contains(directory) {
        return true;
    }
    let mut prefix = Vec::with_capacity(directory.len() + 1);
    prefix.extend_from_slice(directory);
    prefix.push(b'/');
    tracked_paths
        .range::<[u8], _>((
            std::ops::Bound::Included(prefix.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .is_some_and(|path| path.starts_with(&prefix))
}

pub(crate) fn is_same_path(left: &Path, right: &Path) -> bool {
    left == right
}

/// Whether `path`'s final component is `.git`. Git never lists a `.git` entry at
/// any depth (a repository's own `.git`, a submodule gitlink file, or an embedded
/// repository's `.git` directory) as untracked content.
pub(crate) fn is_dot_git_entry(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new(".git"))
}

/// Whether `path` is a directory containing an embedded repository's `.git`
/// *directory*, or a `.git` file whose `gitdir:` pointer resolves to an
/// existing directory (a submodule worktree). Git treats both as a repository
/// boundary (listing the directory as `dir/`); an *invalid* `.git` file (no
/// resolvable `gitdir:` target) is not a boundary — Git descends into the
/// directory and lists its other untracked contents normally.
pub(crate) fn is_nested_repository_boundary(path: &Path, git_dir: &Path) -> bool {
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        if is_same_path(&dot_git, git_dir) {
            return false;
        }
        return true;
    }
    sley_diff_merge::gitlink_git_dir(path).is_some_and(|embedded| !is_same_path(&embedded, git_dir))
}

pub(crate) fn active_repository_worktree_dir(path: &Path, git_dir: &Path) -> bool {
    sley_diff_merge::gitlink_git_dir(path).is_some_and(|embedded| is_same_path(&embedded, git_dir))
}

/// Whether `path` is an embedded repository's `.git` directory or a path inside it.
pub(crate) fn is_embedded_git_internals(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if matches!(component, std::path::Component::Normal(name) if name == ".git")
            && current != root
            && current.join(".git").is_dir()
        {
            return true;
        }
        current.push(component);
    }
    false
}

pub(crate) fn git_path_has_symlink_parent(worktree_root: &Path, git_path: &[u8]) -> Result<bool> {
    let text =
        std::str::from_utf8(git_path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let mut current = worktree_root.to_path_buf();
    let mut components = text.split('/').peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(err) => return Err(err.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn worktree_entry_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        0o040000
    } else {
        file_mode(metadata)
    }
}

pub(crate) fn worktree_path(root: &Path, path: &[u8]) -> Result<PathBuf> {
    let text = std::str::from_utf8(path).map_err(|err| GitError::InvalidPath(err.to_string()))?;
    let relative = PathBuf::from(text);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidPath(format!(
            "invalid worktree path {text}"
        )));
    }
    Ok(root.join(relative))
}

pub(crate) fn remove_worktree_file(root: &Path, path: &[u8]) -> Result<()> {
    let file = worktree_path(root, path)?;
    if !file.exists() {
        return Ok(());
    }
    if file.is_dir() {
        // A tracked path that is a directory on disk is a gitlink: upstream
        // checkout/reset never recurses into a submodule's working tree. It
        // rmdirs the path when empty (remove_scheduled_dirs) and leaves a
        // populated submodule in place.
        match fs::remove_dir(&file) {
            Ok(()) => prune_empty_parents(root, file.parent())?,
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }
    fs::remove_file(&file)?;
    prune_empty_parents(root, file.parent())?;
    Ok(())
}

pub(crate) fn prune_empty_parents(root: &Path, mut dir: Option<&Path>) -> Result<()> {
    while let Some(path) = dir {
        if path == root || path_is_original_cwd(path) {
            break;
        }
        match fs::remove_dir(path) {
            Ok(()) => dir = path.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => dir = path.parent(),
            Err(err) if err.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

pub(crate) fn original_cwd_absolute() -> Option<PathBuf> {
    let cwd = sley_core::original_cwd().or_else(|| env::current_dir().ok())?;
    Some(fs::canonicalize(&cwd).unwrap_or(cwd))
}

pub(crate) fn path_is_original_cwd(path: &Path) -> bool {
    let Some(cwd) = original_cwd_absolute() else {
        return false;
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path == cwd
}

pub(crate) fn original_cwd_is_inside(path: &Path) -> bool {
    let Some(cwd) = original_cwd_absolute() else {
        return false;
    };
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    cwd == path || cwd.starts_with(&path)
}

pub(crate) fn refuse_if_current_working_directory_becomes_file(
    worktree_root: &Path,
    target_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
) -> Result<()> {
    for (path, entry) in target_entries {
        if sley_index::is_gitlink(entry.mode) || (entry.mode & 0o170000) == 0o040000 {
            continue;
        }
        let path = worktree_path(worktree_root, path)?;
        if path_is_original_cwd(&path)
            && fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir())
        {
            return refuse_remove_current_working_directory(&path);
        }
    }
    Ok(())
}

pub(crate) fn refuse_remove_current_working_directory(path: &Path) -> Result<()> {
    eprintln!(
        "error: Refusing to remove the current working directory:\n{}",
        path.display()
    );
    Err(GitError::Exit(128))
}

pub(crate) fn git_tree_entry_cmp(
    left_name: &[u8],
    left_mode: u32,
    right_name: &[u8],
    right_mode: u32,
) -> Ordering {
    let shared = left_name.len().min(right_name.len());
    let name_order = left_name[..shared].cmp(&right_name[..shared]);
    if name_order != Ordering::Equal {
        return name_order;
    }
    let left_end = left_name.len() == shared;
    let right_end = right_name.len() == shared;
    match (left_end, right_end) {
        (true, true) => Ordering::Equal,
        (true, false) => tree_name_terminator(left_mode).cmp(&right_name[shared]),
        (false, true) => left_name[shared].cmp(&tree_name_terminator(right_mode)),
        (false, false) => Ordering::Equal,
    }
}

pub(crate) fn tree_name_terminator(mode: u32) -> u8 {
    if mode == 0o040000 { b'/' } else { 0 }
}

#[cfg(unix)]
pub(crate) fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100755
    } else {
        0o100644
    }
}

#[cfg(not(unix))]
pub(crate) fn file_mode(_metadata: &fs::Metadata) -> u32 {
    0o100644
}

/// The blob content git stores for a symlink: the raw bytes of the link target
/// exactly as `readlink(2)` returns them. On Unix the target is an opaque byte
/// string, so we take the `OsStr` bytes verbatim (no UTF-8 round-trip, no path
/// re-componentization that could rewrite separators).
#[cfg(unix)]
pub(crate) fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let target = fs::read_link(path)?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
pub(crate) fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    // git normalizes symlink targets to forward slashes on platforms whose
    // native separator is `\`.
    Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
}

pub(crate) fn git_path_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
            path.display()
        )));
    }
    Ok(path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes())
}

pub(crate) fn normalize_absolute_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(_)
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn absolute_path_lexically(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_absolute_path_lexically(path)
    } else {
        normalize_absolute_path_lexically(&cwd.join(path))
    }
}

pub(crate) fn repo_path_to_os_path(path: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
    }

    #[cfg(not(unix))]
    {
        let path = std::str::from_utf8(path)
            .map_err(|_| GitError::InvalidPath("index path is not utf8".into()))?;
        Ok(path.split('/').collect())
    }
}

pub(crate) fn git_path_to_relative_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|err| GitError::InvalidPath(format!("invalid utf-8 index path: {err}")))?;
    Ok(path.split('/').collect())
}

pub(crate) fn path_has_trailing_separator(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}
