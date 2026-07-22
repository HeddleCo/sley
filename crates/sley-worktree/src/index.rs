//! Index mutation: add/update-index, refresh, split-index, resolve-undo, cacheinfo/index-info, and write-tree.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::attributes::*;
use crate::checkout::*;
use crate::filter::*;
use crate::ignore::*;
use crate::index_io::*;
use crate::status::*;
use crate::types_admin::*;

/// git's `INDEX_FORMAT_DEFAULT` (read-cache.c).
const INDEX_FORMAT_DEFAULT: u32 = 3;

/// One index-row mutation produced by an apply-like worktree operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyIndexMutation {
    /// Replace every stage of `path` with one fresh stage-zero entry.
    Upsert {
        path: Vec<u8>,
        mode: u32,
        oid: ObjectId,
    },
    /// Remove every index entry at `path`.
    Remove { path: Vec<u8> },
}

impl ApplyIndexMutation {
    /// Repository-relative path affected by this mutation.
    pub fn path(&self) -> &[u8] {
        match self {
            Self::Upsert { path, .. } | Self::Remove { path } => path,
        }
    }
}

/// Controls for applying a batch of patch-driven index mutations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyIndexOptions {
    /// Invalidate the cache-tree extension after changing index rows.
    pub invalidate_cache_tree: bool,
}

impl Default for ApplyIndexOptions {
    fn default() -> Self {
        Self {
            invalidate_cache_tree: true,
        }
    }
}

/// Deterministic outcome of an index-mutation batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyIndexOutcome {
    /// Paths touched, in caller mutation order.
    pub touched_paths: Vec<Vec<u8>>,
    /// Number of mutations applied.
    pub mutation_count: usize,
}

/// Apply patch-driven upserts/removals and restore canonical index ordering.
///
/// This is the reusable index half of `apply --index`/`--cached`: all stages at
/// an affected path are replaced or removed, fresh entries carry zero stat
/// data until the caller's worktree refresh, and stale cache-tree data is
/// invalidated once after the complete batch.
pub fn apply_index_mutations(
    index: &mut Index,
    mutations: &[ApplyIndexMutation],
    options: ApplyIndexOptions,
) -> Result<ApplyIndexOutcome> {
    for mutation in mutations {
        index
            .entries
            .retain(|entry| entry.path.as_bytes() != mutation.path());
        if let ApplyIndexMutation::Upsert { path, mode, oid } = mutation {
            index.entries.push(IndexEntry {
                ctime_seconds: 0,
                ctime_nanoseconds: 0,
                mtime_seconds: 0,
                mtime_nanoseconds: 0,
                dev: 0,
                ino: 0,
                mode: *mode,
                uid: 0,
                gid: 0,
                size: 0,
                oid: *oid,
                flags: (path.len().min(0x0fff)) as u16,
                flags_extended: 0,
                path: BString::from(path.as_slice()),
            });
        }
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    if options.invalidate_cache_tree && !mutations.is_empty() {
        index.set_cache_tree(None)?;
    }
    Ok(ApplyIndexOutcome {
        touched_paths: mutations
            .iter()
            .map(|mutation| mutation.path().to_vec())
            .collect(),
        mutation_count: mutations.len(),
    })
}

#[cfg(test)]
mod apply_index_mutation_tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_raw(ObjectFormat::Sha1, &[byte; 20]).expect("valid sha1")
    }

    fn entry(path: &[u8], stage: u16, byte: u8) -> IndexEntry {
        IndexEntry {
            ctime_seconds: 1,
            ctime_nanoseconds: 2,
            mtime_seconds: 3,
            mtime_nanoseconds: 4,
            dev: 5,
            ino: 6,
            mode: 0o100644,
            uid: 7,
            gid: 8,
            size: 9,
            oid: oid(byte),
            flags: (path.len() as u16) | (stage << 12),
            flags_extended: 0,
            path: BString::from(path),
        }
    }

    #[test]
    fn batch_replaces_all_stages_sorts_and_reports_paths() {
        let mut index = Index {
            version: 2,
            entries: vec![entry(b"z", 0, 1), entry(b"a", 2, 2), entry(b"a", 3, 3)],
            extensions: Vec::new(),
            checksum: None,
        };
        let mutations = vec![
            ApplyIndexMutation::Upsert {
                path: b"a".to_vec(),
                mode: 0o100755,
                oid: oid(4),
            },
            ApplyIndexMutation::Remove {
                path: b"z".to_vec(),
            },
        ];
        let outcome = apply_index_mutations(&mut index, &mutations, ApplyIndexOptions::default())
            .expect("apply mutations");
        assert_eq!(outcome.mutation_count, 2);
        assert_eq!(outcome.touched_paths, vec![b"a".to_vec(), b"z".to_vec()]);
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].path.as_bytes(), b"a");
        assert_eq!(index.entries[0].mode, 0o100755);
        assert_eq!(index.entries[0].oid, oid(4));
        assert_eq!(index.entries[0].mtime_seconds, 0);
    }
}

/// Pick the base version for a freshly-created index, mirroring git's
/// `get_index_format_default`: `GIT_INDEX_VERSION` wins when set (warning +
/// default on a malformed / out-of-range value), otherwise `feature.manyFiles`
/// (→4) then an `index.version` override (warning on out-of-range). The writer's
/// `normalize_index_version_for_extended_flags` later collapses 2/3 by
/// extended-flag need; a chosen version 4 is preserved.
fn fresh_index_default_version(git_dir: &Path) -> u32 {
    if let Some(raw) = env::var_os("GIT_INDEX_VERSION") {
        let raw = raw.to_string_lossy();
        return match raw.parse::<u32>() {
            Ok(version) if (2..=4).contains(&version) => version,
            _ => {
                eprintln!(
                    "warning: GIT_INDEX_VERSION set, but the value is invalid.\nUsing version {INDEX_FORMAT_DEFAULT}"
                );
                INDEX_FORMAT_DEFAULT
            }
        };
    }
    let config = sley_config::read_repo_config(git_dir, None).unwrap_or_default();
    let mut version = if config
        .get_bool("feature", None, "manyFiles")
        .unwrap_or(false)
    {
        4
    } else {
        INDEX_FORMAT_DEFAULT
    };
    if let Some(raw) = config.get("index", None, "version") {
        match raw.trim().parse::<i64>() {
            Ok(value) if (2..=4).contains(&value) => version = value as u32,
            _ => {
                eprintln!(
                    "warning: index.version set, but the value is invalid.\nUsing version {INDEX_FORMAT_DEFAULT}"
                );
                return INDEX_FORMAT_DEFAULT;
            }
        }
    }
    version
}

/// Whether `config` requests a null trailing index hash. git's repo-settings:
/// `feature.manyFiles` defaults skip-hash on, and an explicit `index.skipHash`
/// (last) overrides it.
pub fn index_skip_hash_from_config(config: &GitConfig) -> bool {
    let many_files = config
        .get_bool("feature", None, "manyFiles")
        .unwrap_or(false);
    config
        .get_bool("index", None, "skipHash")
        .unwrap_or(many_files)
}

/// Overwrite the trailing `format.raw_len()` checksum bytes with zeroes (the
/// null oid), for an `index.skipHash` write.
fn zero_trailing_index_hash(bytes: &mut [u8], format: ObjectFormat) {
    let raw = format.raw_len();
    let len = bytes.len();
    if len >= raw {
        bytes[len - raw..].fill(0);
    }
}

/// Load the repository index, or materialize a fresh empty index whose version
/// is chosen by [`fresh_index_default_version`]. Used by the `add` /
/// `update-index` entrypoints so a brand-new index honors
/// `GIT_INDEX_VERSION` / `index.version` / `feature.manyFiles`, like git.
fn read_index_or_fresh(git_dir: &Path, format: ObjectFormat) -> Result<Index> {
    match read_repository_index(git_dir, format)? {
        Some(index) => Ok(index),
        None => {
            let mut index = empty_index();
            index.version = fresh_index_default_version(git_dir);
            Ok(index)
        }
    }
}

pub fn add_paths_to_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<UpdateIndexResult> {
    update_index_paths(
        worktree_root,
        git_dir,
        format,
        paths,
        UpdateIndexOptions {
            add: true,
            remove: false,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: false,
        },
    )
}

pub fn update_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index = read_index_or_fresh(git_dir, format)?;
    update_index_paths_with_index(worktree_root, git_dir, format, index, paths, options)
}

pub fn update_index_paths_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    index: Index,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Result<UpdateIndexResult> {
    let ordered = ordered_paths_from_plain(paths, options);
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        index,
        &ordered,
        options,
        None,
        false,
        IndexCleanMode::Normal,
    )
}

/// Stamp a single uniform mode (from a batch-wide [`UpdateIndexOptions`]) onto
/// every path. Used by the `git add`-style callers that genuinely apply one
/// mode to all paths; the positional `git update-index <flag> <path>...` path
/// instead snapshots a distinct mode per path in the CLI parse walk.
pub(crate) fn ordered_paths_from_plain(
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Vec<UpdateIndexPath> {
    let mode = options.path_mode();
    paths
        .iter()
        .map(|path| UpdateIndexPath {
            path: path.clone(),
            mode,
        })
        .collect()
}

/// Stage an ordered list of paths, each carrying its own `--chmod` state, and
/// (under `verbose`) print the `add`/`remove`/`chmod` action lines inline in
/// command-line order. This is the entry point `git update-index <path>...`
/// uses so that `--chmod=+x A --chmod=-x B --verbose` produces the interleaved
/// `add 'A'` / `chmod +x 'A'` / `add 'B'` / `chmod -x 'B'` output git emits.
pub fn update_index_ordered_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[UpdateIndexPath],
    options: UpdateIndexOptions,
    config: &GitConfig,
    verbose: bool,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index = read_index_or_fresh(git_dir, format)?;
    update_index_ordered_paths_filtered_with_index(
        worktree_root,
        git_dir,
        format,
        index,
        paths,
        options,
        config,
        verbose,
    )
}

pub fn update_index_ordered_paths_filtered_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    index: Index,
    paths: &[UpdateIndexPath],
    options: UpdateIndexOptions,
    config: &GitConfig,
    verbose: bool,
) -> Result<UpdateIndexResult> {
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        index,
        paths,
        options,
        Some(config),
        verbose,
        IndexCleanMode::Normal,
    )
}

/// Like [`add_paths_to_index`], but runs the configured content filters
/// (`core.autocrlf`/`text`/`eol` EOL conversion and `filter.<name>.clean`
/// drivers) on each file's contents before hashing it into the object store.
///
/// `config` is the repository config used to resolve the filters; pass the
/// parsed `<git_dir>/config` (the orchestrator typically already has this).
pub fn add_paths_to_index_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    update_index_paths_filtered(
        worktree_root,
        git_dir,
        format,
        paths,
        UpdateIndexOptions {
            add: true,
            remove: false,
            force_remove: false,
            chmod: None,
            info_only: false,
            ignore_skip_worktree_entries: false,
            allow_skip_worktree_entries: false,
        },
        config,
    )
}

/// Like [`update_index_paths`], but applies the clean-side content filters (see
/// [`apply_clean_filter`]) to file contents before they are hashed/written.
pub fn update_index_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index = read_index_or_fresh(git_dir, format)?;
    update_index_paths_filtered_with_index(
        worktree_root,
        git_dir,
        format,
        index,
        paths,
        options,
        config,
    )
}

pub fn update_index_paths_filtered_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    index: Index,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    let ordered = ordered_paths_from_plain(paths, options);
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        index,
        &ordered,
        options,
        Some(config),
        false,
        IndexCleanMode::Normal,
    )
}

/// Reapply the configured clean conversion to tracked paths even when their
/// current index blobs contain CRLF. This is the engine operation behind
/// `git add --renormalize`: unlike a normal add, it deliberately bypasses the
/// "new safer autocrlf" safeguard while preserving the regular filtered index
/// update and object-writing path.
pub fn renormalize_index_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    config: &GitConfig,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index = read_index_or_fresh(git_dir, format)?;
    let options = UpdateIndexOptions {
        add: true,
        remove: true,
        force_remove: false,
        chmod: None,
        info_only: false,
        ignore_skip_worktree_entries: false,
        allow_skip_worktree_entries: false,
    };
    let ordered = ordered_paths_from_plain(paths, options);
    update_index_paths_impl(
        worktree_root.as_ref(),
        git_dir,
        format,
        index,
        &ordered,
        options,
        Some(config),
        false,
        IndexCleanMode::Renormalize,
    )
}

pub fn add_update_all_tracked_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    clean_config: &GitConfig,
) -> Result<Vec<AddUpdateTrackedAction>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let index_mtime = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| file_mtime_parts(&metadata));
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let prechecks =
        tracked_only_non_clean_prechecks_parallel(worktree_root, &index, &stat_cache, false)?;
    // Unmerged paths are skipped by the stage-0-only precheck above, but git's
    // bare `add -u` resolves them too. Collect each conflicted path once (the
    // index is sorted by (path, stage), so consecutive dedup yields unique
    // paths) and stage the worktree content after the regular pass.
    let unmerged_paths: Vec<Vec<u8>> = {
        let mut paths = index
            .entries
            .iter()
            .filter(|entry| entry.stage() != Stage::Normal)
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect::<Vec<_>>();
        paths.dedup();
        paths
    };
    if prechecks.is_empty() && unmerged_paths.is_empty() {
        return Ok(Vec::new());
    }

    let pending = prechecks
        .into_iter()
        .map(|precheck| match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                (precheck, index.entries[idx].path.as_bytes().to_vec())
            }
            TrackedOnlyPrecheck::Slow(idx) => {
                (precheck, index.entries[idx].path.as_bytes().to_vec())
            }
        })
        .collect::<Vec<_>>();
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut actions = Vec::new();
    let mut index_dirty = false;
    let mut clean_filter = None;
    let trust_filemode = trust_executable_bit(clean_config);
    for (precheck, path) in pending {
        match precheck {
            TrackedOnlyPrecheck::Deleted(_) => {
                if remove_index_entries_with_path(&mut index.entries, &path) {
                    actions.push(AddUpdateTrackedAction::Remove(path));
                    index_dirty = true;
                }
            }
            TrackedOnlyPrecheck::Slow(_) => {
                let (action, dirty) = add_update_tracked_path(
                    worktree_root,
                    git_dir,
                    format,
                    Some(clean_config),
                    trust_filemode,
                    &odb,
                    &stat_cache,
                    &mut clean_filter,
                    &mut index,
                    &path,
                )?;
                index_dirty |= dirty;
                if let Some(action) = action {
                    actions.push(action);
                }
            }
        }
    }

    for path in unmerged_paths {
        let (action, dirty) = add_update_tracked_path(
            worktree_root,
            git_dir,
            format,
            Some(clean_config),
            trust_filemode,
            &odb,
            &stat_cache,
            &mut clean_filter,
            &mut index,
            &path,
        )?;
        index_dirty |= dirty;
        if let Some(action) = action {
            actions.push(action);
        }
    }

    if index_dirty {
        normalize_index_version_for_extended_flags(&mut index);
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
        write_repository_index_ref(git_dir, format, &index)?;
    }
    Ok(actions)
}

/// Collect the worktree changes that a regular `add` invocation can stage,
/// relative to a caller-owned index.
///
/// Unlike general status, this operation deliberately does not compare the
/// index to `HEAD`: staged differences are irrelevant when deciding what a
/// subsequent add can update. That distinction also lets a sparse index keep
/// absent, out-of-cone trees collapsed. Their sparse-directory entries act as
/// tracked boundaries for the untracked walk, while the tracked precheck treats
/// an absent skip-worktree directory as clean.
pub fn collect_index_worktree_status_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    index: &Index,
) -> Result<Vec<ShortStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index_mtime = fs::metadata(index_path)
        .ok()
        .and_then(|metadata| file_mtime_parts(&metadata));
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let sparse_checkout_active = sparse_checkout_active_for_status(git_dir, index);
    let mut entries = short_status_tracked_only_head_matches_index_parallel(
        worktree_root,
        git_dir,
        format,
        index,
        &stat_cache,
        sparse_checkout_active,
        StatusUntrackedMode::All,
    )?;
    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
    let untracked = status_untracked_paths_from_index(
        worktree_root,
        git_dir,
        index,
        &stat_cache,
        &mut ignores,
        StatusUntrackedMode::All,
        None,
    )?;
    append_untracked_status_entries(&mut entries, untracked);
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

/// Add-specific name retained for callers that use the index/worktree query to
/// resolve staging actions.
pub fn collect_add_worktree_status_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    index: &Index,
) -> Result<Vec<ShortStatusEntry>> {
    collect_index_worktree_status_with_index(worktree_root, git_dir, format, index)
}

pub fn add_exact_tracked_path_from_disk(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    git_path: &[u8],
    ignore_removal: bool,
    config_parameters_env: Option<&str>,
) -> Result<AddExactTrackedPathResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AddExactTrackedPathResult::Unsupported);
        }
        Err(err) => return Err(err.into()),
    };
    let mut index_bytes = fs::read(&index_path)?;
    let Some(raw) = raw_exact_index_entry(&index_bytes, format, git_path)? else {
        return Ok(AddExactTrackedPathResult::Unsupported);
    };
    if !raw_exact_entry_can_patch(&raw, git_path) {
        return Ok(AddExactTrackedPathResult::Unsupported);
    }
    if !raw_index_extensions_are_filterable(&index_bytes, raw.entries_end, raw.checksum_offset) {
        return Ok(AddExactTrackedPathResult::Unsupported);
    }

    let entry = raw.entry.clone();
    if entry.stage() != Stage::Normal
        || index_entry_skip_worktree(&entry)
        || sley_index::is_gitlink(entry.mode)
    {
        return Ok(AddExactTrackedPathResult::Unsupported);
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
            return Ok(if ignore_removal {
                AddExactTrackedPathResult::Handled(None)
            } else {
                AddExactTrackedPathResult::Unsupported
            });
        }
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();
    if metadata.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(AddExactTrackedPathResult::Unsupported);
    }
    let index_mtime = file_mtime_parts(&index_metadata);
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    if stat_cache.reuse_index_entry(&entry, &metadata).is_some() {
        return Ok(AddExactTrackedPathResult::Handled(None));
    }

    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    let is_symlink = file_type.is_symlink();
    let body = if is_symlink {
        symlink_target_bytes(&absolute)?
    } else {
        let body = fs::read(&absolute)?;
        // Resolve the effective config WITH command-line `-c` / `--config-env`
        // overrides folded in (e.g. upstream t0027's `git -c core.autocrlf=true
        // add`); the plain repo-config reader would drop them and the fast path
        // would convert/warn against the wrong EOL policy.
        let config =
            sley_config::read_repo_config(git_dir, config_parameters_env).unwrap_or_default();
        let mut clean_filter = None;
        let clean_filter =
            tracked_only_clean_filter_with_config(&mut clean_filter, worktree_root, &config);
        clean_filter.read_attributes_for_path(worktree_root, git_path)?;
        let checks =
            clean_filter
                .matcher
                .attributes_for_path(git_path, &clean_filter.requested, false);
        // The current index blob drives git's `has_crlf_in_index` decision for
        // both the "new safer autocrlf" conversion guard and safecrlf warnings.
        // It must therefore be available even when `core.safecrlf=false`.
        let conv_flags = ConvFlags::from_config(&clean_filter.config);
        let index_blob = SafeCrlfIndexBlob::Lookup {
            odb: &odb,
            oid: entry.oid,
        };
        apply_clean_filter_cow_inner(
            &clean_filter.config,
            &checks,
            git_path,
            &body,
            conv_flags,
            index_blob,
            true,
        )?
        .into_owned()
    };
    let object = EncodedObject::new(ObjectType::Blob, body);
    let oid = object.object_id(format)?;
    if oid != entry.oid || entry.is_intent_to_add() {
        odb.write_object(object)?;
    }

    let config = sley_config::read_repo_config(git_dir, config_parameters_env).unwrap_or_default();
    let trust_filemode = trust_executable_bit(&config);
    let mut updated_entry =
        index_entry_from_metadata_with_filemode(entry.path.clone(), oid, &metadata, trust_filemode);
    if is_symlink {
        updated_entry.mode = 0o120000;
    }
    if updated_entry == entry {
        return Ok(AddExactTrackedPathResult::Handled(None));
    }
    if !raw_updated_entry_can_patch(&entry, &updated_entry, git_path) {
        return Ok(AddExactTrackedPathResult::Unsupported);
    }
    patch_raw_index_entry(&mut index_bytes, format, &raw, &updated_entry)?;
    fs::write(index_path, index_bytes)?;
    let changed = updated_entry.oid != entry.oid || updated_entry.mode != entry.mode;
    Ok(AddExactTrackedPathResult::Handled(
        changed.then(|| AddUpdateTrackedAction::Add(git_path.to_vec())),
    ))
}

pub fn add_exact_tracked_path_with_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    mut index: Index,
    git_path: &[u8],
) -> Result<Option<AddUpdateTrackedAction>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let range = index_entries_path_range(&index.entries, git_path);
    if range.len() != 1 {
        return Ok(None);
    }
    let entry = &index.entries[range.start];
    if entry.stage() != Stage::Normal || index_entry_skip_worktree(entry) {
        return Ok(None);
    }
    let index_path = repository_index_path(git_dir);
    let index_mtime = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| file_mtime_parts(&metadata));
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    let trust_filemode = trust_executable_bit_from_git_dir(git_dir, None);
    let mut clean_filter = None;
    let (action, dirty) = add_update_tracked_path(
        worktree_root,
        git_dir,
        format,
        None,
        trust_filemode,
        &odb,
        &stat_cache,
        &mut clean_filter,
        &mut index,
        git_path,
    )?;
    if dirty {
        normalize_index_version_for_extended_flags(&mut index);
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
        write_repository_index_ref(git_dir, format, &index)?;
    }
    Ok(action)
}

pub(crate) struct RawExactIndexEntry {
    version: u32,
    entry: IndexEntry,
    entry_start: usize,
    entries_end: usize,
    checksum_offset: usize,
}

pub(crate) fn raw_exact_index_entry(
    bytes: &[u8],
    format: ObjectFormat,
    git_path: &[u8],
) -> Result<Option<RawExactIndexEntry>> {
    let hash_len = format.raw_len();
    if bytes.len() < 12 + hash_len {
        return Err(GitError::InvalidFormat("index header too short".into()));
    }
    let checksum_offset = bytes.len() - hash_len;
    let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
    let expected_checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
    if actual_checksum != expected_checksum {
        return Err(GitError::InvalidFormat(format!(
            "index checksum mismatch: expected {expected_checksum}, got {actual_checksum}"
        )));
    }
    if &bytes[..4] != b"DIRC" {
        return Err(GitError::InvalidFormat("missing DIRC signature".into()));
    }
    let version = u32_from_be(&bytes[4..8]);
    if !(2..=3).contains(&version) {
        return Ok(None);
    }
    let count = u32_from_be(&bytes[8..12]) as usize;
    let mut offset = 12;
    let mut found = None;
    for _ in 0..count {
        let entry_header_len = 40 + hash_len + 2;
        if checksum_offset.saturating_sub(offset) < entry_header_len {
            return Err(GitError::InvalidFormat("truncated index entry".into()));
        }
        let start = offset;
        let oid_start = offset + 40;
        let oid_end = oid_start + hash_len;
        let flags = u16_from_be(&bytes[oid_end..oid_end + 2]);
        offset = oid_end + 2;
        let flags_extended = if flags & INDEX_FLAG_EXTENDED != 0 {
            if checksum_offset.saturating_sub(offset) < 2 {
                return Err(GitError::InvalidFormat(
                    "truncated index extended flags".into(),
                ));
            }
            let flags_extended = u16_from_be(&bytes[offset..offset + 2]);
            offset += 2;
            flags_extended
        } else {
            0
        };
        let path_start = offset;
        while bytes.get(offset).copied() != Some(0) {
            offset += 1;
            if offset >= checksum_offset {
                return Err(GitError::InvalidFormat("unterminated index path".into()));
            }
        }
        let path = &bytes[path_start..offset];
        offset += 1;
        while (offset - start) % 8 != 0 {
            offset += 1;
            if offset > checksum_offset {
                return Err(GitError::InvalidFormat("truncated index padding".into()));
            }
        }
        if path == git_path {
            if found.is_some() {
                return Ok(None);
            }
            let oid = ObjectId::from_raw(format, &bytes[oid_start..oid_end])?;
            found = Some(RawExactIndexEntry {
                version,
                entry: IndexEntry {
                    ctime_seconds: u32_from_be(&bytes[start..start + 4]),
                    ctime_nanoseconds: u32_from_be(&bytes[start + 4..start + 8]),
                    mtime_seconds: u32_from_be(&bytes[start + 8..start + 12]),
                    mtime_nanoseconds: u32_from_be(&bytes[start + 12..start + 16]),
                    dev: u32_from_be(&bytes[start + 16..start + 20]),
                    ino: u32_from_be(&bytes[start + 20..start + 24]),
                    mode: u32_from_be(&bytes[start + 24..start + 28]),
                    uid: u32_from_be(&bytes[start + 28..start + 32]),
                    gid: u32_from_be(&bytes[start + 32..start + 36]),
                    size: u32_from_be(&bytes[start + 36..start + 40]),
                    oid,
                    flags,
                    flags_extended,
                    path: BString::from(path),
                },
                entry_start: start,
                entries_end: 0,
                checksum_offset,
            });
        } else if found.is_none() && path > git_path {
            return Ok(None);
        }
    }
    if let Some(mut found) = found {
        found.entries_end = offset;
        Ok(Some(found))
    } else {
        Ok(None)
    }
}

pub(crate) fn raw_exact_entry_can_patch(raw: &RawExactIndexEntry, git_path: &[u8]) -> bool {
    raw.version == 2
        && raw.entry.flags_extended == 0
        && raw.entry.flags & INDEX_FLAG_EXTENDED == 0
        && raw.entry.flags == index_flags(git_path.len(), 0)
        && raw.entry.path.as_bytes() == git_path
}

pub(crate) fn raw_updated_entry_can_patch(
    previous: &IndexEntry,
    updated: &IndexEntry,
    git_path: &[u8],
) -> bool {
    updated.path.as_bytes() == git_path
        && updated.flags_extended == 0
        && updated.flags & INDEX_FLAG_EXTENDED == 0
        && updated.flags == previous.flags
}

pub(crate) fn raw_index_extensions_are_filterable(
    bytes: &[u8],
    entries_end: usize,
    checksum_offset: usize,
) -> bool {
    let mut offset = entries_end;
    while offset < checksum_offset {
        if checksum_offset.saturating_sub(offset) < 8 {
            return false;
        }
        let size = u32_from_be(&bytes[offset + 4..offset + 8]) as usize;
        let Some(end) = offset
            .checked_add(8)
            .and_then(|offset| offset.checked_add(size))
        else {
            return false;
        };
        if end > checksum_offset {
            return false;
        }
        offset = end;
    }
    true
}

pub(crate) fn patch_raw_index_entry(
    bytes: &mut Vec<u8>,
    format: ObjectFormat,
    raw: &RawExactIndexEntry,
    entry: &IndexEntry,
) -> Result<()> {
    let hash_len = format.raw_len();
    let start = raw.entry_start;
    bytes[start..start + 4].copy_from_slice(&entry.ctime_seconds.to_be_bytes());
    bytes[start + 4..start + 8].copy_from_slice(&entry.ctime_nanoseconds.to_be_bytes());
    bytes[start + 8..start + 12].copy_from_slice(&entry.mtime_seconds.to_be_bytes());
    bytes[start + 12..start + 16].copy_from_slice(&entry.mtime_nanoseconds.to_be_bytes());
    bytes[start + 16..start + 20].copy_from_slice(&entry.dev.to_be_bytes());
    bytes[start + 20..start + 24].copy_from_slice(&entry.ino.to_be_bytes());
    bytes[start + 24..start + 28].copy_from_slice(&entry.mode.to_be_bytes());
    bytes[start + 28..start + 32].copy_from_slice(&entry.uid.to_be_bytes());
    bytes[start + 32..start + 36].copy_from_slice(&entry.gid.to_be_bytes());
    bytes[start + 36..start + 40].copy_from_slice(&entry.size.to_be_bytes());
    bytes[start + 40..start + 40 + hash_len].copy_from_slice(entry.oid.as_bytes());
    bytes[start + 40 + hash_len..start + 40 + hash_len + 2]
        .copy_from_slice(&entry.flags.to_be_bytes());

    let mut extension_offset = raw.entries_end;
    let mut removed_cache_tree = false;
    let mut rewritten = Vec::new();
    while extension_offset < raw.checksum_offset {
        let signature = &bytes[extension_offset..extension_offset + 4];
        let size = u32_from_be(&bytes[extension_offset + 4..extension_offset + 8]) as usize;
        let end = extension_offset + 8 + size;
        if signature == b"TREE" {
            removed_cache_tree = true;
        } else {
            rewritten.extend_from_slice(&bytes[extension_offset..end]);
        }
        extension_offset = end;
    }

    if removed_cache_tree {
        bytes.truncate(raw.entries_end);
        bytes.extend_from_slice(&rewritten);
        let checksum = sley_core::digest_bytes(format, bytes)?;
        bytes.extend_from_slice(checksum.as_bytes());
    } else {
        let checksum = sley_core::digest_bytes(format, &bytes[..raw.checksum_offset])?;
        bytes[raw.checksum_offset..raw.checksum_offset + hash_len]
            .copy_from_slice(checksum.as_bytes());
    }
    Ok(())
}

pub(crate) fn u32_from_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(crate) fn u16_from_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

pub(crate) fn add_update_tracked_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    clean_config: Option<&GitConfig>,
    trust_filemode: bool,
    odb: &FileObjectDatabase,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
    index: &mut Index,
    git_path: &[u8],
) -> Result<(Option<AddUpdateTrackedAction>, bool)> {
    let range = index_entries_path_range(&index.entries, git_path);
    if range.is_empty() {
        return Ok((None, false));
    }
    let entry = index.entries[range.start].clone();
    // git's `add -u` (and `-A`) also resolves unmerged paths: `add_files_to_cache`
    // stages the worktree content over every conflict stage, collapsing the path
    // to a single stage-0 entry (or removing it when the file is gone). For such a
    // path the cached `entry` is one of the higher stages, so the stat-cache reuse
    // fast path below must be skipped and the staged result must always replace the
    // whole path range. `replace_index_entries_with_entry` already splices out all
    // stages, so reusing this function gives identical clean-filter/symlink/gitlink
    // handling for the resolution.
    let is_unmerged = entry.stage() != Stage::Normal;
    let absolute = worktree_root.join(repo_path_to_os_path(git_path)?);
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            if remove_index_entries_with_path(&mut index.entries, git_path) {
                return Ok((
                    Some(AddUpdateTrackedAction::Remove(git_path.to_vec())),
                    true,
                ));
            }
            return Ok((None, false));
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        if !sley_index::is_gitlink(entry.mode) {
            return Ok((None, false));
        }
        let oid = sley_diff_merge::gitlink_head_oid(&absolute, format).unwrap_or(entry.oid);
        let mut updated_entry = index_entry_from_metadata_with_filemode(
            entry.path.clone(),
            oid,
            &metadata,
            trust_filemode,
        );
        updated_entry.mode = sley_index::GITLINK_MODE;
        let changed =
            is_unmerged || updated_entry.oid != entry.oid || updated_entry.mode != entry.mode;
        if updated_entry != entry {
            replace_index_entries_with_entry(&mut index.entries, updated_entry);
            return Ok((
                changed.then(|| AddUpdateTrackedAction::Add(git_path.to_vec())),
                true,
            ));
        }
        return Ok((None, false));
    }
    if !(metadata.is_file() || metadata.file_type().is_symlink()) {
        return Ok((None, false));
    }
    if !is_unmerged && stat_cache.reuse_index_entry(&entry, &metadata).is_some() {
        return Ok((None, false));
    }

    let is_symlink = metadata.file_type().is_symlink();
    let body = if is_symlink {
        symlink_target_bytes(&absolute)?
    } else {
        let body = fs::read(&absolute)?;
        let clean_filter = match clean_config {
            Some(config) => {
                tracked_only_clean_filter_with_config(clean_filter, worktree_root, config)
            }
            None => tracked_only_clean_filter(clean_filter, worktree_root, git_dir),
        };
        clean_filter.read_attributes_for_path(worktree_root, git_path)?;
        let checks =
            clean_filter
                .matcher
                .attributes_for_path(git_path, &clean_filter.requested, false);
        // The current index blob drives git's `has_crlf_in_index` decision for
        // both the "new safer autocrlf" conversion guard and safecrlf warnings.
        let conv_flags = ConvFlags::from_config(&clean_filter.config);
        let index_blob = SafeCrlfIndexBlob::Lookup {
            odb,
            oid: entry.oid,
        };
        apply_clean_filter_cow_inner(
            &clean_filter.config,
            &checks,
            git_path,
            &body,
            conv_flags,
            index_blob,
            true,
        )?
        .into_owned()
    };
    let object = EncodedObject::new(ObjectType::Blob, body);
    let oid = object.object_id(format)?;
    if oid != entry.oid || entry.is_intent_to_add() {
        odb.write_object(object)?;
    }
    let mut updated_entry =
        index_entry_from_metadata_with_filemode(entry.path.clone(), oid, &metadata, trust_filemode);
    if is_symlink {
        updated_entry.mode = 0o120000;
    }
    let changed = is_unmerged || updated_entry.oid != entry.oid || updated_entry.mode != entry.mode;
    if updated_entry != entry {
        replace_index_entries_with_entry(&mut index.entries, updated_entry);
        return Ok((
            changed.then(|| AddUpdateTrackedAction::Add(git_path.to_vec())),
            true,
        ));
    }
    Ok((None, false))
}

pub(crate) enum UpdateIndexCleanFilter {
    Full(AttributeMatcher),
    PathLocal,
}

pub(crate) fn index_entries_path_range(
    entries: &[IndexEntry],
    path: &[u8],
) -> std::ops::Range<usize> {
    let mut start = match entries.binary_search_by(|entry| entry.path.as_bytes().cmp(path)) {
        Ok(index) => index,
        Err(insert) => return insert..insert,
    };
    while start > 0 && entries[start - 1].path.as_bytes() == path {
        start -= 1;
    }
    let mut end = start;
    while end < entries.len() && entries[end].path.as_bytes() == path {
        end += 1;
    }
    start..end
}

pub(crate) fn remove_index_entries_with_path(entries: &mut Vec<IndexEntry>, path: &[u8]) -> bool {
    let range = index_entries_path_range(entries, path);
    if range.is_empty() {
        return false;
    }
    entries.drain(range);
    true
}

/// Remove every index entry whose path lives *under* `name/` (a strict
/// directory-prefix collision). Mirrors git's `has_file_name`
/// (read-cache.c): when a *file* entry `a/b` is being added, any entry
/// `a/b/...` already in the index would produce a tree that records `a/b`
/// both as a blob and as a tree — `write-tree` would emit a malformed tree.
/// Entries are sorted by path, so the conflicting children form a contiguous
/// run immediately after `name`'s insertion point.
pub(crate) fn remove_index_entries_under_dir(entries: &mut Vec<IndexEntry>, name: &[u8]) {
    let start = match entries.binary_search_by(|entry| entry.path.as_bytes().cmp(name)) {
        Ok(found) => found + 1,
        Err(insert) => insert,
    };
    let mut end = start;
    while end < entries.len() {
        let candidate = entries[end].path.as_bytes();
        // `candidate` is under `name/` iff it is strictly longer, shares the
        // `name` prefix, and the next byte is the path separator.
        if candidate.len() > name.len()
            && candidate[name.len()] == b'/'
            && candidate[..name.len()] == *name
        {
            end += 1;
        } else {
            break;
        }
    }
    if end > start {
        entries.drain(start..end);
    }
}

/// Remove any *file* entry that is a strict directory-prefix of `name` (e.g.
/// when adding `a/b/c`, drop a file entry `a/b` or `a`). Mirrors git's
/// `has_dir_name` (read-cache.c): such an entry would make the resulting tree
/// record the prefix both as a blob and as the directory containing `name`.
/// We walk every parent directory of `name`, longest first; the moment a
/// real subdirectory already exists at a prefix, no shorter prefix can
/// conflict, so we stop early (git's "already matches the sub-directory"
/// trivial optimization).
pub(crate) fn remove_index_dir_name_conflicts(entries: &mut Vec<IndexEntry>, name: &[u8]) {
    let mut slash = name.len();
    // Walk back over each '/' (longest parent dir first) until the path has no
    // more components.
    while let Some(pos) = name[..slash].iter().rposition(|&byte| byte == b'/') {
        slash = pos;
        let prefix = &name[..slash];
        match entries.binary_search_by(|entry| entry.path.as_bytes().cmp(prefix)) {
            Ok(found) => {
                // A file entry sits exactly at this directory prefix — drop it.
                entries.remove(found);
            }
            Err(insert) => {
                // No file at `prefix`. If a child `prefix/...` already exists,
                // the directory is established and nothing at this prefix (or
                // any shorter one) can conflict; stop.
                if insert < entries.len() {
                    let candidate = entries[insert].path.as_bytes();
                    if candidate.len() > prefix.len()
                        && candidate[prefix.len()] == b'/'
                        && candidate[..prefix.len()] == *prefix
                    {
                        break;
                    }
                }
            }
        }
    }
}

pub(crate) fn replace_index_entries_with_entry(entries: &mut Vec<IndexEntry>, entry: IndexEntry) {
    let path = entry.path.as_bytes().to_vec();
    // Enforce directory/file replacement *before* computing the insert
    // position: git's `add_index_entry_with_check` removes the conflicting
    // entries, then recomputes where the new entry lands. Adding the entry
    // as a file drops any `path/...` children; adding it drops any file that
    // is a directory-prefix of `path`. Skipping this leaves a D/F-corrupt
    // index that `write-tree` turns into a malformed tree.
    remove_index_entries_under_dir(entries, &path);
    remove_index_dir_name_conflicts(entries, &path);
    let range = index_entries_path_range(entries, &path);
    if range.is_empty() {
        entries.insert(range.start, entry);
    } else {
        entries.splice(range, [entry]);
    }
}

pub(crate) fn write_index_blob_object(
    odb: &FileObjectDatabase,
    format: ObjectFormat,
    object: EncodedObject,
    large_policy: LargeObjectPolicy,
    pending_large: &mut Vec<(ObjectId, EncodedObject)>,
) -> Result<ObjectId> {
    let oid = object.object_id(format)?;
    if object.object_type == ObjectType::Blob && object.body.len() as u64 >= large_policy.threshold
    {
        if !odb.contains(&oid)? {
            pending_large.push((oid, object));
        }
        return Ok(oid);
    }
    odb.write_object(object)
}

pub(crate) fn write_pending_large_blobs(
    odb: &FileObjectDatabase,
    objects: &[(ObjectId, EncodedObject)],
    policy: LargeObjectPolicy,
) -> Result<()> {
    let Some(limit) = policy.pack_size_limit else {
        return odb.write_blobs_as_pack(objects, policy.compression_level);
    };
    let mut start = 0usize;
    let mut current_size = 0u64;
    for (idx, (_, object)) in objects.iter().enumerate() {
        let estimate = object.body.len() as u64 + 32;
        if idx > start && current_size.saturating_add(estimate) > limit {
            odb.write_blobs_as_pack(&objects[start..idx], policy.compression_level)?;
            start = idx;
            current_size = 0;
        }
        current_size = current_size.saturating_add(estimate);
    }
    if start < objects.len() {
        odb.write_blobs_as_pack(&objects[start..], policy.compression_level)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexCleanMode {
    Normal,
    Renormalize,
}

pub(crate) fn update_index_paths_impl(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut index: Index,
    paths: &[UpdateIndexPath],
    options: UpdateIndexOptions,
    clean_config: Option<&GitConfig>,
    verbose: bool,
    clean_mode: IndexCleanMode,
) -> Result<UpdateIndexResult> {
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut large_policy = LargeObjectPolicy::from_config(git_dir, None)?;
    if let Some(config) = clean_config {
        large_policy.compression_level = pack_compression_level(config);
        large_policy.pack_size_limit = config
            .get("pack", None, "packSizeLimit")
            .and_then(sley_config::parse_config_int)
            .and_then(|value| (value > 0).then_some(value as u64))
            .or(large_policy.pack_size_limit);
    }
    let trust_filemode = clean_config
        .map(trust_executable_bit)
        .unwrap_or_else(|| trust_executable_bit_from_git_dir(git_dir, None));
    let trust_symlinks = clean_config
        .map(trust_symlinks)
        .unwrap_or_else(|| trust_symlinks_from_git_dir(git_dir, None));
    let sparse_checkout_active = sparse_checkout_config_enabled(git_dir)
        || index.is_sparse()
        || index.entries.iter().any(IndexEntry::is_sparse_dir);
    // For small batches, read only each path's `.gitattributes` chain; a
    // whole-worktree matcher can dominate `add -u` when only a few files are
    // dirty in a huge checkout. Large batches still amortize the full matcher.
    let clean_filter = match clean_config {
        Some(_) if paths.len() >= 64 => Some(UpdateIndexCleanFilter::Full(
            AttributeMatcher::from_worktree_root(worktree_root)?,
        )),
        Some(_) => Some(UpdateIndexCleanFilter::PathLocal),
        None => None,
    };
    // git's index-update path (object-file.c `get_conv_flags`) folds in
    // `global_conv_flags_eol`, so `git add`/`commit` emit the `core.safecrlf`
    // round-trip warning (default: warn). It only applies when content filters
    // run at all (i.e. when we have a config).
    let conv_flags = clean_config.map_or(ConvFlags::Off, ConvFlags::from_config);
    let non_atomic_chmod_errors = clean_config.is_some() && options.add && options.remove;
    let requested_filter_attrs = filter_attribute_names();
    let mut updated = Vec::new();
    let mut reports: Vec<String> = Vec::new();
    let mut untracked_cache_invalidation_paths = Vec::new();
    let mut pending_large = Vec::new();
    let mut chmod_error = false;
    for update_path in paths {
        let path = &update_path.path;
        // Each path carries the sticky mode that was in effect when it was
        // parsed on the command line (git processes argv left-to-right). Read
        // the action from the path's own mode, NOT a batch-wide flag, so
        // `--add foo --force-remove bar` adds foo and force-removes bar.
        let path_mode = update_path.mode;
        let path_chmod = path_mode.chmod;
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let absolute = normalize_absolute_path_lexically(&absolute);
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if index_sparse_dir_contains_path(&index, &git_path) {
            expand_sparse_index_directories(&mut index, &odb, format, |directory| {
                git_path.starts_with(directory)
            })?;
        }
        let existing_range = index_entries_path_range(&index.entries, &git_path);
        if path_mode.force_remove {
            record_resolve_undo_for_range(&mut index, format, &git_path, existing_range)?;
            remove_index_entries_with_path(&mut index.entries, &git_path);
            untracked_cache_invalidation_paths.push(git_path.clone());
            // git's update_one() reports `remove` for a --force-remove path.
            reports.push(format!("remove '{}'", String::from_utf8_lossy(&git_path)));
            continue;
        }
        // git's `add_one_path` → `ie_match_stat(..., flags=0)` treats
        // CE_VALID/assume-unchanged as a match, so a dirty assume-unchanged
        // path is a silent no-op (even when named on the command line).
        // `--really-refresh` clears the bit before path processing; plain
        // `--refresh keep.txt` must leave assume-unchanged entries alone.
        if index.entries[existing_range.clone()]
            .iter()
            .any(|entry| entry.flags & INDEX_FLAG_ASSUME_UNCHANGED != 0)
        {
            continue;
        }
        // lstat (not stat): a symlink must be inspected as the link itself, never
        // followed to its target. `Path::exists`/`fs::metadata` both stat through
        // the link, which makes a symlink-to-directory look like a directory
        // (fs::read then fails with "Is a directory") and a symlink-to-file get
        // staged with the target's content + a regular-file mode. git stages a
        // symlink as mode 120000 whose blob is the link target string, regardless
        // of what (if anything) the target resolves to.
        let symlink_metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => Some(metadata),
            // ENOTDIR (a leading path component is now a file, e.g. staging the
            // stale `a/b/c` entry after `a/b` became a regular file in a D/F
            // flip) means the path no longer exists as a file — git's lstat
            // returns ENOTDIR here and treats it exactly like ENOENT. Fold both
            // into the "missing" arm so the `--remove` path drops the stale
            // entry instead of aborting the whole add with an I/O error.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            Err(err) => return Err(err.into()),
        };
        if !options.allow_skip_worktree_entries
            && index.entries[existing_range.clone()]
                .iter()
                .any(index_entry_skip_worktree)
        {
            if path_mode.remove {
                if !options.ignore_skip_worktree_entries {
                    index.entries.drain(existing_range);
                }
                continue;
            }
            if symlink_metadata.is_none()
                || options.ignore_skip_worktree_entries
                || !sparse_checkout_active
            {
                continue;
            }
        }
        let Some(metadata) = symlink_metadata else {
            if path_mode.remove {
                record_resolve_undo_for_range(&mut index, format, &git_path, existing_range)?;
                remove_index_entries_with_path(&mut index.entries, &git_path);
                untracked_cache_invalidation_paths.push(git_path.clone());
                // git's update_one() unconditionally reports `add '<path>'`
                // after process_path(), even when the missing file was removed
                // from the index via the `--remove` (not --force-remove) path.
                reports.push(format!("add '{}'", String::from_utf8_lossy(&git_path)));
                continue;
            }
            print_update_index_path_error(&git_path, "does not exist and --remove not passed");
            return Err(GitError::Exit(128));
        };
        if !path_mode.add && index_entries_path_range(&index.entries, &git_path).is_empty() {
            print_update_index_path_error(
                &git_path,
                "cannot add to the index - missing --add option?",
            );
            return Err(GitError::Exit(128));
        }
        if metadata.is_dir() {
            if path_mode.remove
                && !existing_range.is_empty()
                && sley_diff_merge::gitlink_head_oid(&absolute, format).is_none()
            {
                record_resolve_undo_for_range(
                    &mut index,
                    format,
                    &git_path,
                    existing_range.clone(),
                )?;
                remove_index_entries_with_path(&mut index.entries, &git_path);
                untracked_cache_invalidation_paths.push(git_path.clone());
                reports.push(format!("add '{}'", String::from_utf8_lossy(&git_path)));
                continue;
            }
            // A directory is stageable only as a gitlink: when it is an
            // embedded repository with a commit checked out, git records a
            // mode-160000 entry whose oid is that commit (no object is
            // written). Otherwise it errors — with upstream's exact messages
            // for the embedded-repo-without-commit and plain-directory cases
            // (object-file.c index_path / builtin/update-index.c
            // process_directory).
            let display = String::from_utf8_lossy(&git_path).into_owned();
            let has_dot_git = absolute.join(".git").exists();
            if let Some(submodule_format) = embedded_repo_object_format(&absolute)
                && submodule_format != format
            {
                eprintln!("fatal: cannot add a submodule of a different hash algorithm");
                return Err(GitError::Exit(128));
            }
            let Some(head_oid) = sley_diff_merge::gitlink_head_oid(&absolute, format) else {
                if has_dot_git {
                    if clean_config.is_some() {
                        let display_dir = if display.ends_with('/') {
                            display.clone()
                        } else {
                            format!("{display}/")
                        };
                        eprintln!("error: '{display_dir}' does not have a commit checked out");
                        eprintln!("error: unable to index file '{display_dir}'");
                        eprintln!("fatal: adding files failed");
                    } else {
                        eprintln!("error: '{display}' does not have a commit checked out");
                        eprintln!("fatal: Unable to process path {display}");
                    }
                } else {
                    eprintln!("error: {display}: is a directory - add files inside instead");
                    eprintln!("fatal: Unable to process path {display}");
                }
                return Err(GitError::Exit(128));
            };
            if path_chmod.is_some() {
                eprintln!(
                    "fatal: git update-index: cannot chmod {}x '{display}'",
                    if path_chmod == Some(true) { '+' } else { '-' },
                );
                return Err(GitError::Exit(128));
            }
            let mut entry = index_entry_from_metadata_with_filemode(
                git_path.clone(),
                head_oid,
                &metadata,
                trust_filemode,
            );
            entry.mode = sley_index::GITLINK_MODE;
            reports.push(format!("add '{display}'"));
            record_resolve_undo_for_range(&mut index, format, &git_path, existing_range.clone())?;
            replace_index_entries_with_entry(&mut index.entries, entry);
            untracked_cache_invalidation_paths.push(git_path.clone());
            updated.push(head_oid);
            continue;
        }
        let is_symlink = metadata.file_type().is_symlink();
        let body = if is_symlink {
            // The blob is the raw link target bytes; clean filters never apply to
            // a symlink (git treats it as binary content, not a text path).
            symlink_target_bytes(&absolute)?
        } else {
            let body = fs::read(&absolute)?;
            // Normal add consults the current stage-0 blob for git's "new safer
            // autocrlf" rule. `--renormalize` deliberately bypasses that guard
            // so the clean policy is reapplied to legacy CRLF blobs.
            let index_blob = match clean_mode {
                IndexCleanMode::Renormalize => SafeCrlfIndexBlob::None,
                IndexCleanMode::Normal => {
                    stage0_oid_in_range(&index.entries, existing_range.clone()).map_or(
                        SafeCrlfIndexBlob::None,
                        |oid| SafeCrlfIndexBlob::Lookup { odb: &odb, oid },
                    )
                }
            };
            match (clean_config, &clean_filter) {
                (Some(config), Some(UpdateIndexCleanFilter::Full(matcher))) => {
                    // Identical to `apply_clean_filter`, but reuses the batch's
                    // matcher instead of rebuilding it (and re-walking the tree)
                    // for this path.
                    let checks =
                        matcher.attributes_for_path(&git_path, &requested_filter_attrs, false);
                    apply_clean_filter_cow_inner(
                        config, &checks, &git_path, &body, conv_flags, index_blob, true,
                    )?
                    .into_owned()
                }
                (Some(config), Some(UpdateIndexCleanFilter::PathLocal)) => {
                    let checks = filter_attribute_checks(worktree_root, &git_path)?;
                    apply_clean_filter_cow_inner(
                        config, &checks, &git_path, &body, conv_flags, index_blob, true,
                    )?
                    .into_owned()
                }
                _ => body,
            }
        };
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = if path_mode.info_only {
            object.object_id(format)?
        } else {
            write_index_blob_object(&odb, format, object, large_policy, &mut pending_large)?
        };
        let mut entry = index_entry_from_metadata_with_filemode(
            git_path.clone(),
            oid,
            &metadata,
            trust_filemode,
        );
        if is_symlink {
            entry.mode = 0o120000;
        }
        if let Some(mode) = preferred_index_mode_for_untrusted_worktree(
            &index.entries[existing_range.clone()],
            trust_filemode,
            trust_symlinks,
        ) {
            entry.mode = mode;
        }
        // git's update_one() reports `add` for every staged path (whether the
        // entry is new or an update), then chmod_path() reports the chmod after.
        reports.push(format!("add '{}'", String::from_utf8_lossy(&git_path)));
        if let Some(executable) = path_chmod {
            // git's chmod_path() refuses to flip the executable bit on anything
            // that is not a regular file (a symlink/gitlink has no such bit). It
            // writes the blob first, reports the error, and still writes the
            // other index updates.
            if entry.mode & 0o170000 != 0o100000 {
                eprintln!(
                    "fatal: git update-index: cannot chmod {}x '{}'",
                    if executable { '+' } else { '-' },
                    String::from_utf8_lossy(&git_path)
                );
                if !non_atomic_chmod_errors {
                    return Err(GitError::Exit(128));
                }
                chmod_error = true;
            } else {
                entry.mode = if executable { 0o100755 } else { 0o100644 };
                reports.push(format!(
                    "chmod {}x '{}'",
                    if executable { '+' } else { '-' },
                    String::from_utf8_lossy(&git_path)
                ));
            }
        }
        record_resolve_undo_for_range(&mut index, format, &git_path, existing_range.clone())?;
        replace_index_entries_with_entry(&mut index.entries, entry);
        untracked_cache_invalidation_paths.push(git_path);
        updated.push(oid);
    }
    normalize_index_version_for_extended_flags(&mut index);
    update_cache_tree_for_git_paths(
        &mut index,
        format,
        &odb,
        &untracked_cache_invalidation_paths,
    )?;
    invalidate_untracked_cache_for_git_paths(
        &mut index,
        format,
        &untracked_cache_invalidation_paths,
    )?;
    if !pending_large.is_empty() {
        write_pending_large_blobs(&odb, &pending_large, large_policy)?;
    }
    // git's `index.skipHash` / `feature.manyFiles` decide whether the trailing
    // checksum is written. `clean_config` carries the full effective config
    // (file + command-line `-c`) for the add/update-index callers.
    let skip_hash = clean_config
        .map(index_skip_hash_from_config)
        .unwrap_or(false);
    write_repository_index_ref_skip_hash(git_dir, format, &index, skip_hash)?;
    if verbose {
        let mut stdout = std::io::stdout().lock();
        for line in &reports {
            writeln!(stdout, "{line}")?;
        }
        stdout.flush()?;
    }
    if chmod_error {
        return Err(GitError::Exit(128));
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

pub fn refresh_index_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    quiet: bool,
    ignore_missing: bool,
    really_refresh: bool,
) -> Result<UpdateIndexResult> {
    refresh_index_paths_with_options(
        worktree_root,
        git_dir,
        format,
        paths,
        quiet,
        ignore_missing,
        /* ignore_submodules */ false,
        /* allow_unmerged */ false,
        really_refresh,
    )
}

pub fn refresh_index_paths_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    quiet: bool,
    ignore_missing: bool,
    ignore_submodules: bool,
    allow_unmerged: bool,
    really_refresh: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(UpdateIndexResult {
            entries: 0,
            updated: Vec::new(),
        });
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let trust_filemode = trust_executable_bit_from_git_dir(git_dir, None);
    // git's `update-index --refresh` trusts the cached stat: a stage-0 entry
    // whose size+mtime still match the worktree file (and is not racily clean) is
    // known unchanged, so its content is NOT re-read or re-hashed
    // (read-cache.c `refresh_cache_ent` → `ie_match_stat`). Without this shortcut
    // sley re-hashed every tracked file on every refresh — the 3.2x slowdown in
    // sley#27. We build the cache from the same parsed index + the index file's
    // own mtime (the racy-clean reference) so no extra parse is needed.
    let index_mtime = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| file_mtime_parts(&metadata));
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    let selected_paths = selected_paths.into_iter().collect::<BTreeSet<_>>();
    if selected_paths.is_empty()
        && !really_refresh
        && !index
            .entries
            .iter()
            .any(|entry| entry.flags & INDEX_FLAG_ASSUME_UNCHANGED != 0)
    {
        return refresh_all_index_paths_parallel(
            worktree_root,
            git_dir,
            format,
            index,
            stat_cache,
            quiet,
            ignore_missing,
            ignore_submodules,
            allow_unmerged,
            trust_filemode,
        );
    }
    let mut needs_update = false;
    let mut index_dirty = false;
    for entry in &mut index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        // Pathspec-limited refresh (e.g. `git add --refresh bar`) must only
        // re-stat matching entries. Without this filter every stage-0 entry was
        // refreshed whenever *any* path was selected (t3700 "with pathspec").
        if !selected_paths.is_empty()
            && !git_path_selected(entry.path.as_bytes(), &selected_paths)
        {
            continue;
        }
        if entry.flags & INDEX_FLAG_ASSUME_UNCHANGED != 0 {
            if !really_refresh {
                continue;
            }
            entry.flags &= !INDEX_FLAG_ASSUME_UNCHANGED;
            index_dirty = true;
        }
        let absolute = worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?);
        let Ok(metadata) = fs::symlink_metadata(&absolute) else {
            if ignore_missing {
                continue;
            }
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            continue;
        };
        // git's `refresh_cache_ent` runs `ie_match_stat`, whose `S_IFGITLINK`
        // arm never re-reads content: a gitlink whose worktree path is a
        // directory is up to date (an unpopulated/HEAD-matching submodule), so
        // `--refresh` leaves it untouched and silent. Only a gitlink that is no
        // longer a directory (replaced by a file, or removed) is `TYPE_CHANGED`.
        // This is the single `sley_index::gitlink_stat_verdict` rule; without it
        // the `!is_file()` guard below mis-flagged every populated submodule as
        // "needs update". The populated-HEAD comparison is deliberately left to
        // status/diff (the unpopulated default is clean).
        if sley_index::is_gitlink(entry.mode) {
            match sley_index::gitlink_stat_verdict(&metadata) {
                sley_index::GitlinkStatVerdict::Populated => {
                    if ignore_submodules {
                        continue;
                    }
                    if let Some(oid) = sley_diff_merge::gitlink_head_oid(&absolute, format)
                        && oid != entry.oid
                    {
                        if !quiet {
                            print_update_index_needs_update(entry.path.as_bytes());
                        }
                        needs_update = true;
                    }
                    continue;
                }
                sley_index::GitlinkStatVerdict::TypeChanged => {
                    if !quiet {
                        print_update_index_needs_update(entry.path.as_bytes());
                    }
                    needs_update = true;
                    continue;
                }
            }
        }
        let is_symlink = metadata.file_type().is_symlink();
        if !(metadata.is_file() || is_symlink) {
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            continue;
        }
        // Stat shortcut: when the cached stat proves the file is unchanged since
        // it was staged, its content hashes to the cached oid by construction
        // (see `IndexStatCache`'s safety invariant). Skip the read+hash and just
        // refresh the stat fields from current metadata — byte-identical to the
        // clean arm below, since the oid stamped is the cached one and the
        // metadata is the same one that re-stamp would read.
        if stat_cache.reuse_index_entry(entry, &metadata).is_some() {
            continue;
        }
        let body = if is_symlink {
            symlink_target_bytes(&absolute)?
        } else {
            fs::read(&absolute)?
        };
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format)?;
        let worktree_mode = if is_symlink {
            sley_index::SYMLINK_MODE
        } else {
            file_mode_with_trust(&metadata, trust_filemode)
        };
        if oid != entry.oid || worktree_mode != entry.mode {
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            if really_refresh
                && !selected_paths.is_empty()
                && selected_paths.contains(entry.path.as_bytes())
            {
                let mut updated_entry = index_entry_from_metadata_with_filemode(
                    entry.path.clone(),
                    oid,
                    &metadata,
                    trust_filemode,
                );
                if is_symlink {
                    updated_entry.mode = sley_index::SYMLINK_MODE;
                }
                if updated_entry != *entry {
                    *entry = updated_entry;
                    index_dirty = true;
                }
            }
            continue;
        }
        let mut updated_entry = index_entry_from_metadata_with_filemode(
            entry.path.clone(),
            oid,
            &metadata,
            trust_filemode,
        );
        if is_symlink {
            updated_entry.mode = sley_index::SYMLINK_MODE;
        }
        if updated_entry != *entry {
            *entry = updated_entry;
            index_dirty = true;
        }
    }
    if index_dirty {
        write_repository_index_ref(git_dir, format, &index)?;
    }
    if needs_update && !quiet {
        return Err(GitError::Exit(1));
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub(crate) fn refresh_all_index_paths_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut index: Index,
    stat_cache: IndexStatCache,
    quiet: bool,
    ignore_missing: bool,
    ignore_submodules: bool,
    _allow_unmerged: bool,
    trust_filemode: bool,
) -> Result<UpdateIndexResult> {
    let prechecks =
        tracked_only_non_clean_prechecks_parallel(worktree_root, &index, &stat_cache, false)?;
    let mut needs_update = false;
    let mut index_dirty = false;
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                if ignore_missing {
                    continue;
                }
                if !quiet {
                    print_update_index_needs_update(index.entries[idx].path.as_bytes());
                }
                needs_update = true;
            }
            TrackedOnlyPrecheck::Slow(idx) => {
                let entry = &mut index.entries[idx];
                let path = entry.path.as_bytes().to_vec();
                let absolute = worktree_root.join(repo_path_to_os_path(&path)?);
                let Ok(metadata) = fs::symlink_metadata(&absolute) else {
                    if ignore_missing {
                        continue;
                    }
                    if !quiet {
                        print_update_index_needs_update(&path);
                    }
                    needs_update = true;
                    continue;
                };
                // Gitlink: never re-read; a directory on disk is up to date (the
                // single `sley_index::gitlink_stat_verdict` rule, matching the
                // serial path above). Only a type-changed gitlink needs update.
                if sley_index::is_gitlink(entry.mode) {
                    match sley_index::gitlink_stat_verdict(&metadata) {
                        sley_index::GitlinkStatVerdict::Populated => {
                            if ignore_submodules {
                                continue;
                            }
                            if let Some(oid) = sley_diff_merge::gitlink_head_oid(&absolute, format)
                                && oid != entry.oid
                            {
                                if !quiet {
                                    print_update_index_needs_update(&path);
                                }
                                needs_update = true;
                            }
                            continue;
                        }
                        sley_index::GitlinkStatVerdict::TypeChanged => {
                            if !quiet {
                                print_update_index_needs_update(&path);
                            }
                            needs_update = true;
                            continue;
                        }
                    }
                }
                let is_symlink = metadata.file_type().is_symlink();
                if !(metadata.is_file() || is_symlink) {
                    if !quiet {
                        print_update_index_needs_update(&path);
                    }
                    needs_update = true;
                    continue;
                }
                if stat_cache.reuse_index_entry(entry, &metadata).is_some() {
                    continue;
                }
                let body = if is_symlink {
                    symlink_target_bytes(&absolute)?
                } else {
                    fs::read(&absolute)?
                };
                let object = EncodedObject::new(ObjectType::Blob, body);
                let oid = object.object_id(format)?;
                let worktree_mode = if is_symlink {
                    sley_index::SYMLINK_MODE
                } else {
                    file_mode_with_trust(&metadata, trust_filemode)
                };
                if oid != entry.oid || worktree_mode != entry.mode {
                    if !quiet {
                        print_update_index_needs_update(&path);
                    }
                    needs_update = true;
                    continue;
                }
                let mut updated_entry = index_entry_from_metadata_with_filemode(
                    entry.path.clone(),
                    oid,
                    &metadata,
                    trust_filemode,
                );
                if is_symlink {
                    updated_entry.mode = sley_index::SYMLINK_MODE;
                }
                if updated_entry != *entry {
                    *entry = updated_entry;
                    index_dirty = true;
                }
            }
        }
    }
    if index_dirty {
        write_repository_index_ref(git_dir, format, &index)?;
    }
    if needs_update && !quiet {
        return Err(GitError::Exit(1));
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn update_index_again(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: UpdateIndexOptions,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let (entry_count, again_paths) =
        select_index_again_paths(worktree_root, git_dir, format, paths)?;
    if again_paths.is_empty() {
        return Ok(UpdateIndexResult {
            entries: entry_count,
            updated: Vec::new(),
        });
    }
    update_index_paths(worktree_root, git_dir, format, &again_paths, options)
}

/// Apply `--[no-]skip-worktree` only to the paths selected by `--again`.
///
/// Git treats `--again` as a path selector for the other update-index mode in
/// effect.  It must not re-stage a missing out-of-cone file when the requested
/// operation is only to clear its skip-worktree bit.
pub fn set_index_skip_worktree_again(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    skip_worktree: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let (entry_count, again_paths) =
        select_index_again_paths(worktree_root, git_dir, format, paths)?;
    if again_paths.is_empty() {
        return Ok(UpdateIndexResult {
            entries: entry_count,
            updated: Vec::new(),
        });
    }
    set_index_skip_worktree_paths(worktree_root, git_dir, format, &again_paths, skip_worktree)
}

fn select_index_again_paths(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<(usize, Vec<PathBuf>)> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok((0, Vec::new()));
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // `--again` compares logical index entries with HEAD.  A collapsed sparse
    // directory is an index storage node, not a tracked path; comparing it to
    // the flattened HEAD map incorrectly selects every out-of-cone directory
    // for a worktree update.  Expand only the temporary comparison view, with
    // no observable full-index transition.
    expand_sparse_index_in_memory(&mut index, &db, format)?;
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    let selected_paths = selected_git_paths(worktree_root, paths)?;
    let mut again_paths = Vec::new();
    for entry in &index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        if !selected_paths.is_empty() && !git_path_selected(entry.path.as_bytes(), &selected_paths)
        {
            continue;
        }
        let differs_from_head = match head_entries.get(entry.path.as_bytes()) {
            Some(head_entry) => head_entry.oid != entry.oid || head_entry.mode != entry.mode,
            None => true,
        };
        if differs_from_head {
            again_paths.push(worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?));
        }
    }
    Ok((index.entries.len(), again_paths))
}

pub fn set_index_assume_unchanged_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    assume_unchanged: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let sparse = active_sparse_checkout(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if index.is_sparse() {
        expand_sparse_index(&mut index, &db, format)?;
    }
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if let Some(entry) = index.entries.iter_mut().find(|entry| entry.path == path) {
            if assume_unchanged {
                entry.flags |= INDEX_FLAG_ASSUME_UNCHANGED;
            } else {
                entry.flags &= !INDEX_FLAG_ASSUME_UNCHANGED;
            }
        }
    }
    normalize_index_version_for_extended_flags(&mut index);
    if let Some((sparse, mode)) = sparse
        && sparse.sparse_index
    {
        let matcher = SparseMatcher::new(&sparse, mode);
        collapse_to_sparse_index(&mut index, &matcher, &db, format)?;
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub(crate) fn selected_git_paths(
    worktree_root: &Path,
    paths: &[PathBuf],
) -> Result<BTreeSet<Vec<u8>>> {
    paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect()
}

pub(crate) fn git_path_selected(path: &[u8], selected_paths: &BTreeSet<Vec<u8>>) -> bool {
    selected_paths
        .iter()
        .any(|selected| path == selected || index_entry_is_under_path(path, selected))
}

pub fn set_index_skip_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    skip_worktree: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let sparse = active_sparse_checkout(git_dir)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if index.is_sparse() {
        expand_sparse_index(&mut index, &db, format)?;
    }
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if let Some(entry) = index.entries.iter_mut().find(|entry| entry.path == path) {
            if skip_worktree {
                entry.flags |= INDEX_FLAG_EXTENDED;
                entry.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
            } else {
                entry.flags_extended &= !INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
                if entry.flags_extended == 0 {
                    entry.flags &= !INDEX_FLAG_EXTENDED;
                }
            }
        }
    }
    normalize_index_version_for_extended_flags(&mut index);
    if let Some((sparse, mode)) = sparse
        && sparse.sparse_index
    {
        let matcher = SparseMatcher::new(&sparse, mode);
        collapse_to_sparse_index(&mut index, &matcher, &db, format)?;
    }
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn set_index_fsmonitor_valid_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    _fsmonitor_valid: bool,
) -> Result<UpdateIndexResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let selected_paths = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                worktree_root.join(path)
            };
            let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
                GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
            })?;
            git_path_bytes(relative)
        })
        .collect::<Result<Vec<_>>>()?;
    for path in selected_paths {
        if !index.entries.iter().any(|entry| entry.path == path) {
            eprintln!(
                "fatal: Unable to mark file {}",
                String::from_utf8_lossy(&path)
            );
            return Err(GitError::Exit(128));
        }
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn set_index_version(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    version: u32,
    verbose: bool,
) -> Result<UpdateIndexResult> {
    if !matches!(version, 2..=4) {
        return Err(GitError::Unsupported(format!(
            "update-index currently supports --index-version 2, 3, or 4, got {version}"
        )));
    }
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    // git reports the transition unconditionally under --verbose, even when the
    // requested version equals the current one ("was 4, set to 4").
    let previous = index.version;
    if verbose {
        println!("index-version: was {previous}, set to {version}");
    }
    index.version = version;
    normalize_index_version_for_extended_flags(&mut index);
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn force_write_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    normalize_index_version_for_extended_flags(&mut index);
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn enable_untracked_cache(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<()> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        empty_index()
    };
    let ident = untracked_cache_ident(worktree_root);
    let dir_flags = untracked_cache_dir_flags(StatusUntrackedMode::Normal);
    let cache = match index.untracked_cache(format)? {
        Some(mut cache) if cache.ident == ident => {
            cache.dir_flags = dir_flags;
            cache
        }
        _ => UntrackedCache::new(format, ident, dir_flags),
    };
    index.set_untracked_cache(format, Some(&cache))?;
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(())
}

pub fn disable_untracked_cache(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<()> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut index = Index::parse(&fs::read(&index_path)?, format)?;
    index.set_untracked_cache(format, None)?;
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(())
}

pub fn refresh_untracked_cache_after_status(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    config: &GitConfig,
    untracked_mode: StatusUntrackedMode,
) -> Result<()> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(());
    }
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let untracked_cache_setting = config.get("core", None, "untrackedCache");
    match untracked_cache_setting {
        Some("keep") | None => {
            if !repository_index_has_extension(git_dir, format, b"UNTR")? {
                return Ok(());
            }
        }
        Some("false" | "no" | "off" | "0") | Some("true" | "yes" | "on" | "1") => {}
        Some(_) => {
            if !repository_index_has_extension(git_dir, format, b"UNTR")? {
                return Ok(());
            }
        }
    }
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        empty_index()
    };
    match untracked_cache_setting {
        Some("false") | Some("no") | Some("off") | Some("0") => {
            index.set_untracked_cache(format, None)?;
            write_repository_index_ref(git_dir, format, &index)?;
            return Ok(());
        }
        Some("true") | Some("yes") | Some("on") | Some("1") => {}
        Some("keep") | None => {
            if index.untracked_cache(format)?.is_none() {
                return Ok(());
            }
        }
        Some(_) => {
            if index.untracked_cache(format)?.is_none() {
                return Ok(());
            }
        }
    }
    let old_cache = index.untracked_cache(format).ok().flatten();
    let ident = untracked_cache_ident(worktree_root);
    if old_cache.as_ref().is_some_and(|cache| cache.ident != ident) {
        eprintln!("warning: untracked cache is disabled on this system or location");
        emit_untracked_cache_bypass_trace();
        return Ok(());
    }
    let cache = build_untracked_cache(worktree_root, git_dir, format, &index, untracked_mode)?;
    emit_untracked_cache_trace(old_cache.as_ref(), &cache);
    index.set_untracked_cache(format, Some(&cache))?;
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(())
}

pub(crate) fn repository_index_has_extension(
    git_dir: &Path,
    format: ObjectFormat,
    signature: &[u8; 4],
) -> Result<bool> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(false);
    }
    let bytes = read_borrowed_index_bytes(&index_path)?;
    sley_index::Index::bytes_have_extension(bytes.as_ref(), format, signature)
}

pub fn emit_untracked_cache_bypass_trace() {
    sley_core::trace2::perf_read_directory_data("path", "");
}

pub(crate) fn index_extensions_without_cache_tree(extensions: &[u8]) -> Vec<u8> {
    let mut offset = 0;
    let mut filtered = Vec::new();
    while offset < extensions.len() {
        if extensions.len().saturating_sub(offset) < 8 {
            return Vec::new();
        }
        let signature = &extensions[offset..offset + 4];
        let size = u32::from_be_bytes([
            extensions[offset + 4],
            extensions[offset + 5],
            extensions[offset + 6],
            extensions[offset + 7],
        ]) as usize;
        let end = offset + 8 + size;
        if end > extensions.len() {
            return Vec::new();
        }
        if signature != b"TREE" {
            filtered.extend_from_slice(&extensions[offset..end]);
        }
        offset = end;
    }
    filtered
}

pub(crate) fn update_cache_tree_for_git_paths(
    index: &mut Index,
    format: ObjectFormat,
    odb: &FileObjectDatabase,
    paths: &[Vec<u8>],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    match index.cache_tree(format) {
        Ok(Some(mut cache_tree)) if !cache_tree_has_invalid_untouched(&cache_tree, b"", paths) => {
            for path in paths {
                invalidate_cache_tree_path(&mut cache_tree, path);
            }
            index.set_cache_tree(Some(&cache_tree))?;
            return Ok(());
        }
        Ok(_) => {}
        Err(_) => {
            index.extensions = index_extensions_without_cache_tree(&index.extensions);
            return Ok(());
        }
    }
    let cache_tree = match build_cache_tree_for_index_update(&index.entries, b"", paths, odb) {
        Ok(cache_tree) => cache_tree,
        Err(_) => {
            index.extensions = index_extensions_without_cache_tree(&index.extensions);
            return Ok(());
        }
    };
    index.set_cache_tree(Some(&cache_tree))?;
    Ok(())
}

/// Rebuild the index cache-tree (`TREE`) extension from the current entries.
///
/// This is for commands that have just materialized a complete staged tree
/// (read-tree, checkout/reset, write-tree/commit). Incremental index mutators
/// should use `update_cache_tree_for_git_paths` so untouched valid subtrees
/// can be preserved and touched paths become invalid, matching git-add.
pub fn refresh_cache_tree(index: &mut Index, odb: &FileObjectDatabase) {
    match build_cache_tree_for_index_update(&index.entries, b"", &[], odb) {
        Ok(cache_tree) => {
            if index.set_cache_tree(Some(&cache_tree)).is_err() {
                index.extensions = index_extensions_without_cache_tree(&index.extensions);
            }
        }
        Err(_) => {
            index.extensions = index_extensions_without_cache_tree(&index.extensions);
        }
    }
}

pub fn refresh_repository_cache_tree(
    git_dir: &Path,
    format: ObjectFormat,
    odb: &FileObjectDatabase,
) -> Result<()> {
    let index_path = repository_index_path(git_dir);
    let Ok(bytes) = fs::read(&index_path) else {
        return Ok(());
    };
    let mut index = Index::parse(&bytes, format)?;
    if index.split_index_link(format)?.is_some() {
        return Ok(());
    }
    refresh_cache_tree(&mut index, odb);
    write_repository_index_ref(git_dir, format, &index)
}

fn invalidate_cache_tree_path(tree: &mut CacheTree, path: &[u8]) {
    tree.entry_count = -1;
    tree.oid = None;

    let mut rest = path;
    while rest.first() == Some(&b'/') {
        rest = &rest[1..];
    }
    if rest.is_empty() {
        return;
    }
    let split = rest
        .iter()
        .position(|byte| *byte == b'/')
        .unwrap_or(rest.len());
    let component = &rest[..split];
    let remaining = if split == rest.len() {
        &[][..]
    } else {
        &rest[split + 1..]
    };
    if let Some(child) = tree
        .subtrees
        .iter_mut()
        .find(|child| child.name.as_slice() == component)
    {
        invalidate_cache_tree_path(&mut child.tree, remaining);
    }
}

fn cache_tree_has_invalid_untouched(tree: &CacheTree, prefix: &[u8], paths: &[Vec<u8>]) -> bool {
    if !cache_tree_prefix_touched(prefix, paths) && (tree.entry_count < 0 || tree.oid.is_none()) {
        return true;
    }
    tree.subtrees.iter().any(|child| {
        let mut child_prefix = Vec::with_capacity(prefix.len() + child.name.len() + 1);
        child_prefix.extend_from_slice(prefix);
        child_prefix.extend_from_slice(&child.name);
        child_prefix.push(b'/');
        cache_tree_has_invalid_untouched(&child.tree, &child_prefix, paths)
    })
}

fn build_cache_tree_for_index_update(
    entries: &[IndexEntry],
    prefix: &[u8],
    invalid_paths: &[Vec<u8>],
    odb: &FileObjectDatabase,
) -> Result<CacheTree> {
    let mut invalid = cache_tree_prefix_touched(prefix, invalid_paths);
    let mut subtrees = Vec::new();
    let mut tree_entries = Vec::new();
    let mut index = 0usize;
    while index < entries.len() {
        let entry = &entries[index];
        if entry.stage() != Stage::Normal || entry.is_intent_to_add() {
            invalid = true;
            index += 1;
            continue;
        }
        let path = entry.path.as_bytes();
        let Some(remainder) = path.strip_prefix(prefix) else {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        };
        if remainder.is_empty() || remainder[0] == b'/' {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        }
        if entry.mode == SPARSE_DIR_MODE
            && let Some(name) = remainder.strip_suffix(b"/")
            && !name.is_empty()
            && !name.contains(&b'/')
        {
            if !invalid {
                tree_entries.push(TreeEntry {
                    mode: SPARSE_DIR_MODE,
                    name: BString::from(name),
                    oid: entry.oid,
                });
            }
            index += 1;
            continue;
        }
        if let Some(slash) = remainder.iter().position(|byte| *byte == b'/') {
            let name = &remainder[..slash];
            if name.is_empty() {
                return Err(GitError::InvalidPath(format!(
                    "invalid index path {}",
                    String::from_utf8_lossy(path)
                )));
            }
            let start = index;
            index += 1;
            while index < entries.len()
                && same_tree_component(entries[index].path.as_bytes(), prefix, name)?
            {
                index += 1;
            }
            let mut child_prefix = Vec::with_capacity(prefix.len() + name.len() + 1);
            child_prefix.extend_from_slice(prefix);
            child_prefix.extend_from_slice(name);
            child_prefix.push(b'/');
            let child_tree = build_cache_tree_for_index_update(
                &entries[start..index],
                &child_prefix,
                invalid_paths,
                odb,
            )?;
            if !invalid {
                if let Some(oid) = child_tree.oid {
                    tree_entries.push(TreeEntry {
                        mode: 0o040000,
                        name: BString::from(name),
                        oid,
                    });
                } else {
                    invalid = true;
                }
            }
            subtrees.push(sley_index::CacheTreeChild {
                name: name.to_vec(),
                tree: child_tree,
            });
            continue;
        }
        if !invalid {
            tree_entries.push(TreeEntry {
                mode: entry.mode,
                name: BString::from(remainder),
                oid: entry.oid,
            });
        }
        index += 1;
    }

    if invalid {
        return Ok(CacheTree {
            entry_count: -1,
            oid: None,
            subtrees,
        });
    }
    tree_entries.sort_by(|left, right| {
        git_tree_entry_cmp(
            left.name.as_bytes(),
            left.mode,
            right.name.as_bytes(),
            right.mode,
        )
    });
    let oid = odb.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: tree_entries,
        }
        .write(),
    ))?;
    let entry_count = i32::try_from(entries.len())
        .map_err(|_| GitError::InvalidFormat("cache-tree entry count overflow".into()))?;
    Ok(CacheTree {
        entry_count,
        oid: Some(oid),
        subtrees,
    })
}

fn cache_tree_prefix_touched(prefix: &[u8], paths: &[Vec<u8>]) -> bool {
    if paths.is_empty() {
        return false;
    }
    if prefix.is_empty() {
        return true;
    }
    let dir = prefix.strip_suffix(b"/").unwrap_or(prefix);
    paths
        .iter()
        .any(|path| path.as_slice() == dir || path.starts_with(prefix))
}

#[derive(Clone)]
pub(crate) struct ResolveUndoRecord {
    pub(crate) path: Vec<u8>,
    pub(crate) stages: [Option<(u32, ObjectId)>; 3],
}

pub(crate) fn record_resolve_undo_for_path(
    index: &mut Index,
    format: ObjectFormat,
    path: &[u8],
    entries: &[IndexEntry],
) -> Result<()> {
    let mut stages = [None, None, None];
    for entry in entries {
        match entry.stage() {
            Stage::Base => stages[0] = Some((entry.mode, entry.oid)),
            Stage::Ours => stages[1] = Some((entry.mode, entry.oid)),
            Stage::Theirs => stages[2] = Some((entry.mode, entry.oid)),
            Stage::Normal => {}
        }
    }
    if stages.iter().all(Option::is_none) {
        return Ok(());
    }
    let mut records = parse_resolve_undo_records(index.extension(b"REUC")?, format)?;
    records.retain(|record| record.path.as_slice() != path);
    records.push(ResolveUndoRecord {
        path: path.to_vec(),
        stages,
    });
    records.sort_by(|left, right| left.path.cmp(&right.path));
    set_resolve_undo_extension(index, &records)
}

pub(crate) fn record_resolve_undo_for_range(
    index: &mut Index,
    format: ObjectFormat,
    path: &[u8],
    range: Range<usize>,
) -> Result<()> {
    if range.is_empty() {
        return Ok(());
    }
    let entries = index.entries[range].to_vec();
    record_resolve_undo_for_path(index, format, path, &entries)
}

pub(crate) fn parse_resolve_undo_records(
    body: Option<&[u8]>,
    format: ObjectFormat,
) -> Result<Vec<ResolveUndoRecord>> {
    let Some(body) = body else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let path_end = body[offset..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| GitError::InvalidFormat("truncated REUC path".into()))?
            + offset;
        let path = body[offset..path_end].to_vec();
        offset = path_end + 1;

        let mut modes = [0u32; 3];
        for mode in &mut modes {
            let mode_end = body[offset..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| GitError::InvalidFormat("truncated REUC mode".into()))?
                + offset;
            let text = std::str::from_utf8(&body[offset..mode_end])
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            *mode = u32::from_str_radix(text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid REUC mode".into()))?;
            offset = mode_end + 1;
        }

        let mut stages = [None, None, None];
        for (idx, mode) in modes.into_iter().enumerate() {
            if mode == 0 {
                continue;
            }
            let end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("REUC oid length overflow".into()))?;
            if end > body.len() {
                return Err(GitError::InvalidFormat("truncated REUC oid".into()));
            }
            stages[idx] = Some((mode, ObjectId::from_raw(format, &body[offset..end])?));
            offset = end;
        }
        records.push(ResolveUndoRecord { path, stages });
    }
    Ok(records)
}

pub(crate) fn set_resolve_undo_extension(
    index: &mut Index,
    records: &[ResolveUndoRecord],
) -> Result<()> {
    let mut body = Vec::new();
    for record in records {
        body.extend_from_slice(&record.path);
        body.push(0);
        for stage in record.stages {
            match stage {
                Some((mode, _)) => body.extend_from_slice(format!("{mode:o}").as_bytes()),
                None => body.push(b'0'),
            }
            body.push(0);
        }
        for (_, oid) in record.stages.into_iter().flatten() {
            body.extend_from_slice(oid.as_bytes());
        }
    }

    let chunks = index.extension_chunks()?;
    let mut rebuilt = Vec::with_capacity(index.extensions.len() + body.len() + 8);
    let mut replaced = false;
    for (signature, chunk_body) in chunks {
        if &signature == b"REUC" {
            if !body.is_empty() {
                append_index_extension(&mut rebuilt, b"REUC", &body)?;
            }
            replaced = true;
        } else {
            append_index_extension(&mut rebuilt, &signature, chunk_body)?;
        }
    }
    if !replaced && !body.is_empty() {
        append_index_extension(&mut rebuilt, b"REUC", &body)?;
    }
    index.extensions = rebuilt;
    Ok(())
}

pub fn clear_resolve_undo(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<()> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    match fs::read(&index_path) {
        Ok(bytes) => {
            let mut index = Index::parse(&bytes, format)?;
            set_resolve_undo_extension(&mut index, &[])?;
            write_repository_index_ref(git_dir, format, &index)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn append_index_extension(
    out: &mut Vec<u8>,
    signature: &[u8; 4],
    body: &[u8],
) -> Result<()> {
    let len = u32::try_from(body.len())
        .map_err(|_| GitError::InvalidFormat("index extension body too large".into()))?;
    out.extend_from_slice(signature);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

pub(crate) fn index_extensions_without_split_index_link(extensions: &[u8]) -> Vec<u8> {
    let mut offset = 0;
    let mut filtered = Vec::new();
    while offset < extensions.len() {
        if extensions.len().saturating_sub(offset) < 8 {
            filtered.extend_from_slice(&extensions[offset..]);
            break;
        }
        let signature = &extensions[offset..offset + 4];
        let len = u32::from_be_bytes([
            extensions[offset + 4],
            extensions[offset + 5],
            extensions[offset + 6],
            extensions[offset + 7],
        ]) as usize;
        let end = offset.saturating_add(8).saturating_add(len);
        if end > extensions.len() {
            filtered.extend_from_slice(&extensions[offset..]);
            break;
        }
        if signature != b"link" {
            filtered.extend_from_slice(&extensions[offset..end]);
        }
        offset = end;
    }
    filtered
}

pub(crate) fn preserved_index_extensions(git_dir: &Path, format: ObjectFormat) -> Result<Vec<u8>> {
    let index_path = repository_index_path(git_dir);
    match fs::read(&index_path) {
        Ok(bytes) if bytes.is_empty() => Ok(Vec::new()),
        Ok(bytes) => {
            let index = Index::parse(&bytes, format)?;
            Ok(index_extensions_without_cache_tree_or_resolve_undo(
                &index.extensions,
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn index_extensions_without_cache_tree_or_resolve_undo(extensions: &[u8]) -> Vec<u8> {
    let mut filtered = Vec::new();
    let mut offset = 0usize;
    while offset + 8 <= extensions.len() {
        let signature = &extensions[offset..offset + 4];
        let len = u32::from_be_bytes([
            extensions[offset + 4],
            extensions[offset + 5],
            extensions[offset + 6],
            extensions[offset + 7],
        ]) as usize;
        let end = offset + 8 + len;
        if end > extensions.len() {
            filtered.extend_from_slice(&extensions[offset..]);
            break;
        }
        if signature != b"TREE" && signature != b"REUC" {
            filtered.extend_from_slice(&extensions[offset..end]);
        }
        offset = end;
    }
    filtered
}

pub(crate) fn repository_index_is_split(git_dir: &Path, format: ObjectFormat) -> Result<bool> {
    let index_path = repository_index_path(git_dir);
    match fs::read(index_path) {
        Ok(bytes) if bytes.is_empty() => Ok(false),
        Ok(bytes) => Ok(Index::parse(&bytes, format)?
            .split_index_link(format)?
            .is_some()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn git_test_split_index_enabled() -> bool {
    env::var("GIT_TEST_SPLIT_INDEX")
        .ok()
        .is_some_and(|value| !matches!(value.as_str(), "" | "0" | "false" | "False" | "FALSE"))
}

pub fn write_repository_index(git_dir: &Path, format: ObjectFormat, index: Index) -> Result<()> {
    let split = index.split_index_link(format)?.is_some()
        || repository_index_is_split(git_dir, format)?
        || git_test_split_index_enabled();
    write_repository_index_ref_with_split(git_dir, format, &index, split)
}

pub fn write_repository_index_ref(
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
) -> Result<()> {
    let split = index.split_index_link(format)?.is_some()
        || repository_index_is_split(git_dir, format)?
        || git_test_split_index_enabled();
    write_repository_index_ref_with_split(git_dir, format, index, split)
}

/// As [`write_repository_index_ref`], but `skip_hash` writes a null trailing
/// checksum (git's `index.skipHash` / `feature.manyFiles`).
pub fn write_repository_index_ref_skip_hash(
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    skip_hash: bool,
) -> Result<()> {
    let split = index.split_index_link(format)?.is_some()
        || repository_index_is_split(git_dir, format)?
        || git_test_split_index_enabled();
    write_repository_index_ref_with_split_skip_hash(git_dir, format, index, split, skip_hash)
}

pub(crate) fn write_repository_index_ref_with_split(
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    split: bool,
) -> Result<()> {
    write_repository_index_ref_with_split_skip_hash(git_dir, format, index, split, false)
}

/// As [`write_repository_index_ref_with_split`], but `skip_hash` leaves the
/// trailing checksum as the null oid (git's `index.skipHash`). Only the
/// non-split write honors it (the case `add`/`update-index` exercise); the
/// reader accepts a null trailing hash regardless (see verify_hdr).
pub(crate) fn write_repository_index_ref_with_split_skip_hash(
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    split: bool,
    skip_hash: bool,
) -> Result<()> {
    let index_path = repository_index_path(git_dir);
    if !split || alternate_index_output_path(git_dir, &index_path) {
        let smudged_entries = racily_clean_entry_indexes_before_write(git_dir, format, index)?;
        let extensions = if index.split_index_link(format)?.is_some() {
            Cow::Owned(index_extensions_without_split_index_link(&index.extensions))
        } else {
            Cow::Borrowed(index.extensions.as_slice())
        };
        let mut bytes = if smudged_entries.is_empty() && matches!(extensions, Cow::Borrowed(_)) {
            index.write(format)?
        } else {
            write_index_with_entry_size_overrides(format, index, &smudged_entries, &extensions)?
        };
        if skip_hash {
            zero_trailing_index_hash(&mut bytes, format);
        }
        fs::write(&index_path, bytes)?;
        apply_index_shared_file_mode(git_dir, &index_path, None)?;
        return Ok(());
    }

    if let Some(link) = index.split_index_link(format)?
        && !link.base_oid.is_null()
        && let Some(base) = read_shared_index_for_link(git_dir, &index_path, format, &link)?
        && !split_index_delta_exceeds_threshold(git_dir, index, &base)
    {
        let (entries, link) = split_index_delta_entries(index, &base, &link)?;
        let extensions = index_extensions_without_split_index_link(
            &index_extensions_without_cache_tree(&index.extensions),
        );
        let mut primary = Index {
            version: index.version,
            entries,
            extensions,
            checksum: None,
        };
        primary.set_split_index_link(Some(&link))?;
        fs::write(&index_path, primary.write(format)?)?;
        apply_index_shared_file_mode(git_dir, &index_path, None)?;
        return Ok(());
    }

    let mode_source = fs::metadata(&index_path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut shared = index.clone();
    smudge_racily_clean_entries_before_write(git_dir, format, &mut shared)?;
    shared.clear_split_index_link()?;
    shared.extensions = index_extensions_without_cache_tree(&shared.extensions);
    let shared_bytes = shared.write(format)?;
    let shared_oid = index_checksum_from_bytes(format, &shared_bytes)?;
    let shared_path = git_dir.join(format!("sharedindex.{shared_oid}"));
    if !shared_path.exists() {
        fs::write(&shared_path, &shared_bytes)?;
    }
    apply_index_shared_file_mode(git_dir, &shared_path, mode_source.as_ref())?;
    clean_shared_index_files(git_dir, shared_oid)?;

    let mut primary = Index {
        version: index.version,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    };
    primary.set_split_index_link(Some(&SplitIndexLink::new(shared_oid)))?;
    fs::write(&index_path, primary.write(format)?)?;
    apply_index_shared_file_mode(git_dir, &index_path, mode_source.as_ref())?;
    Ok(())
}

pub(crate) fn alternate_index_output_path(git_dir: &Path, index_path: &Path) -> bool {
    env::var_os("GIT_INDEX_FILE").is_some() && index_path != git_dir.join("index")
}

pub(crate) fn clean_shared_index_files(git_dir: &Path, current_oid: ObjectId) -> Result<()> {
    let Some(expire_before) = shared_index_expire_before(git_dir) else {
        return Ok(());
    };
    let current_name = format!("sharedindex.{current_oid}");
    let mut expired = Vec::new();
    for entry in fs::read_dir(git_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("sharedindex.") || name == current_name {
            continue;
        }
        let metadata = entry.metadata()?;
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified <= expire_before {
            expired.push((modified, entry.path()));
        }
    }
    expired.sort_by_key(|(modified, _)| *modified);
    let delete_count = expired.len().saturating_sub(1);
    for (_, path) in expired.into_iter().take(delete_count) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

pub(crate) fn shared_index_expire_before(git_dir: &Path) -> Option<SystemTime> {
    let value = sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| {
            config
                .get("splitIndex", None, "sharedIndexExpire")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "2.weeks.ago".to_string());
    let value = value.trim();
    if value.eq_ignore_ascii_case("never") {
        return None;
    }
    if value.eq_ignore_ascii_case("now") {
        return Some(SystemTime::now());
    }
    if let Some(days) = value
        .strip_suffix(".days.ago")
        .or_else(|| value.strip_suffix(".day.ago"))
        .and_then(|days| days.parse::<u64>().ok())
    {
        return SystemTime::now().checked_sub(Duration::from_secs(days * 24 * 60 * 60));
    }
    if let Some(weeks) = value
        .strip_suffix(".weeks.ago")
        .or_else(|| value.strip_suffix(".week.ago"))
        .and_then(|weeks| weeks.parse::<u64>().ok())
    {
        return SystemTime::now().checked_sub(Duration::from_secs(weeks * 7 * 24 * 60 * 60));
    }
    SystemTime::now().checked_sub(Duration::from_secs(14 * 24 * 60 * 60))
}

pub(crate) fn apply_index_shared_file_mode(
    git_dir: &Path,
    path: &Path,
    mode_source: Option<&fs::Permissions>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let current = fs::metadata(path)?.permissions();
        let source_mode = mode_source
            .map(fs::Permissions::mode)
            .unwrap_or_else(|| current.mode());
        let mode = sley_config::read_repo_config(git_dir, None)
            .ok()
            .and_then(|config| {
                config
                    .get("core", None, "sharedRepository")
                    .and_then(|value| shared_repository_file_mode(value, source_mode))
            })
            .unwrap_or(source_mode & 0o7777);
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = git_dir;
        let _ = path;
        let _ = mode_source;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn shared_repository_file_mode(value: &str, source_mode: u32) -> Option<u32> {
    match value {
        "umask" | "false" | "no" | "off" | "0" => None,
        "group" | "true" | "yes" | "on" | "1" => Some((source_mode | 0o660) & 0o7777),
        "all" | "world" | "everybody" | "2" | "3" => Some((source_mode | 0o664) & 0o7777),
        value => {
            let parsed = u32::from_str_radix(value, 8).ok()?;
            (parsed & 0o600 == 0o600).then_some(parsed & 0o666)
        }
    }
}

pub(crate) fn read_shared_index_for_link(
    git_dir: &Path,
    index_path: &Path,
    format: ObjectFormat,
    link: &SplitIndexLink,
) -> Result<Option<Index>> {
    let name = format!("sharedindex.{}", link.base_oid);
    let bytes = match fs::read(git_dir.join(&name)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let alternate = index_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&name);
            match fs::read(alternate) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err.into()),
            }
        }
        Err(err) => return Err(err.into()),
    };
    let base = Index::parse(&bytes, format)?;
    if base.checksum != Some(link.base_oid) {
        return Ok(None);
    }
    Ok(Some(base))
}

pub(crate) fn split_index_delta_exceeds_threshold(
    git_dir: &Path,
    index: &Index,
    base: &Index,
) -> bool {
    let max_percent = sley_config::read_repo_config(git_dir, None)
        .ok()
        .and_then(|config| {
            config
                .get("splitIndex", None, "maxPercentChange")
                .and_then(|value| value.parse::<i64>().ok())
        })
        .unwrap_or(20);
    match max_percent {
        0 => return true,
        100.. => return false,
        value if value < 0 => {}
        _ => {}
    }
    let not_shared = count_entries_not_shared_with_base(index, base);
    (index.entries.len() as i64) * max_percent < (not_shared as i64) * 100
}

pub(crate) fn count_entries_not_shared_with_base(index: &Index, base: &Index) -> usize {
    index
        .entries
        .iter()
        .filter(|entry| {
            base.entries
                .binary_search_by(|base_entry| compare_index_key(base_entry, entry))
                .is_err()
        })
        .count()
}

pub(crate) fn split_index_delta_entries(
    index: &Index,
    base: &Index,
    previous_link: &SplitIndexLink,
) -> Result<(Vec<IndexEntry>, SplitIndexLink)> {
    let mut delete_positions = Vec::new();
    let mut replace_positions = Vec::new();
    let mut replacements = Vec::new();
    let mut additions = Vec::new();
    let mut base_pos = 0usize;
    let mut index_pos = 0usize;
    while base_pos < base.entries.len() && index_pos < index.entries.len() {
        match compare_index_key(&base.entries[base_pos], &index.entries[index_pos]) {
            Ordering::Equal => {
                if previous_link
                    .delete_positions
                    .binary_search(&(base_pos as u32))
                    .is_ok()
                {
                    delete_positions.push(base_pos as u32);
                    additions.push(index.entries[index_pos].clone());
                } else if !index_entry_content_eq(
                    &base.entries[base_pos],
                    &index.entries[index_pos],
                ) {
                    replace_positions.push(base_pos as u32);
                    let mut replacement = index.entries[index_pos].clone();
                    replacement.path = BString::from(Vec::<u8>::new());
                    replacement.refresh_name_length();
                    replacements.push(replacement);
                }
                base_pos += 1;
                index_pos += 1;
            }
            Ordering::Less => {
                delete_positions.push(base_pos as u32);
                base_pos += 1;
            }
            Ordering::Greater => {
                additions.push(index.entries[index_pos].clone());
                index_pos += 1;
            }
        }
    }
    while base_pos < base.entries.len() {
        delete_positions.push(base_pos as u32);
        base_pos += 1;
    }
    while index_pos < index.entries.len() {
        additions.push(index.entries[index_pos].clone());
        index_pos += 1;
    }
    replacements.extend(additions);
    Ok((
        replacements,
        SplitIndexLink {
            base_oid: previous_link.base_oid,
            delete_positions,
            replace_positions,
        },
    ))
}

pub(crate) fn compare_index_key(left: &IndexEntry, right: &IndexEntry) -> Ordering {
    left.path
        .as_bytes()
        .cmp(right.path.as_bytes())
        .then_with(|| left.stage().as_u16().cmp(&right.stage().as_u16()))
}

pub(crate) fn index_entry_content_eq(left: &IndexEntry, right: &IndexEntry) -> bool {
    const ONDISK_FLAGS: u16 = sley_index::INDEX_FLAG_STAGE_MASK
        | sley_index::INDEX_FLAG_VALID
        | sley_index::INDEX_FLAG_EXTENDED;
    left.ctime_seconds == right.ctime_seconds
        && left.ctime_nanoseconds == right.ctime_nanoseconds
        && left.mtime_seconds == right.mtime_seconds
        && left.mtime_nanoseconds == right.mtime_nanoseconds
        && left.dev == right.dev
        && left.ino == right.ino
        && left.mode == right.mode
        && left.uid == right.uid
        && left.gid == right.gid
        && left.size == right.size
        && left.oid == right.oid
        && (left.flags & ONDISK_FLAGS) == (right.flags & ONDISK_FLAGS)
        && left.flags_extended == right.flags_extended
}

pub(crate) fn write_index_with_entry_size_overrides(
    format: ObjectFormat,
    index: &Index,
    zero_size_entries: &[usize],
    extensions: &[u8],
) -> Result<Vec<u8>> {
    if !(2..=4).contains(&index.version) {
        return Err(GitError::Unsupported(
            "canonical writer currently emits index v2/v3/v4".into(),
        ));
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&index.version.to_be_bytes());
    out.extend_from_slice(&(index.entries.len() as u32).to_be_bytes());
    let mut previous_path = Vec::new();
    for (position, entry) in index.entries.iter().enumerate() {
        let start = out.len();
        out.extend_from_slice(&entry.ctime_seconds.to_be_bytes());
        out.extend_from_slice(&entry.ctime_nanoseconds.to_be_bytes());
        out.extend_from_slice(&entry.mtime_seconds.to_be_bytes());
        out.extend_from_slice(&entry.mtime_nanoseconds.to_be_bytes());
        out.extend_from_slice(&entry.dev.to_be_bytes());
        out.extend_from_slice(&entry.ino.to_be_bytes());
        out.extend_from_slice(&entry.mode.to_be_bytes());
        out.extend_from_slice(&entry.uid.to_be_bytes());
        out.extend_from_slice(&entry.gid.to_be_bytes());
        let size = if zero_size_entries.binary_search(&position).is_ok() {
            0
        } else {
            entry.size
        };
        out.extend_from_slice(&size.to_be_bytes());
        if entry.oid.format() != format {
            return Err(GitError::Unsupported(format!(
                "index writer expects {} ids",
                format.name()
            )));
        }
        out.extend_from_slice(entry.oid.as_bytes());
        let has_extended_flags =
            entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0;
        if has_extended_flags && index.version < 3 {
            return Err(GitError::Unsupported(
                "index extended flags require version 3".into(),
            ));
        }
        let flags = if has_extended_flags {
            entry.flags | INDEX_FLAG_EXTENDED
        } else {
            entry.flags & !INDEX_FLAG_EXTENDED
        };
        out.extend_from_slice(&flags.to_be_bytes());
        if has_extended_flags {
            out.extend_from_slice(&entry.flags_extended.to_be_bytes());
        }
        if index.version == 4 {
            let common_prefix_len = common_prefix_len(&previous_path, entry.path.as_bytes());
            let strip_len = previous_path.len() - common_prefix_len;
            encode_index_v4_path_strip_len(strip_len, &mut out);
            out.extend_from_slice(&entry.path.as_bytes()[common_prefix_len..]);
            out.push(0);
            previous_path = entry.path.as_bytes().to_vec();
        } else {
            out.extend_from_slice(entry.path.as_bytes());
            out.push(0);
            while (out.len() - start) % 8 != 0 {
                out.push(0);
            }
        }
    }
    out.extend_from_slice(extensions);
    let checksum = sley_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok(out)
}

pub(crate) fn encode_index_v4_path_strip_len(strip_len: usize, out: &mut Vec<u8>) {
    let mut bytes = Vec::new();
    bytes.push((strip_len & 0x7f) as u8);
    let mut value = strip_len >> 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    for byte in bytes.iter().rev() {
        out.push(*byte);
    }
}

pub(crate) fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

pub(crate) fn index_checksum_from_bytes(format: ObjectFormat, bytes: &[u8]) -> Result<ObjectId> {
    let hash_len = format.raw_len();
    if bytes.len() < hash_len {
        return Err(GitError::InvalidFormat(
            "index too short for checksum".into(),
        ));
    }
    ObjectId::from_raw(format, &bytes[bytes.len() - hash_len..])
}

pub fn enable_split_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let mut index = read_repository_index(git_dir, format)?.unwrap_or_else(empty_index);
    normalize_index_version_for_extended_flags(&mut index);
    write_repository_index_ref_with_split(git_dir, format, &index, true)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub fn disable_split_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    if !repository_index_path(git_dir).exists() {
        return Ok(UpdateIndexResult {
            entries: 0,
            updated: Vec::new(),
        });
    }
    let mut index = read_repository_index(git_dir, format)?.unwrap_or_else(empty_index);
    normalize_index_version_for_extended_flags(&mut index);
    write_repository_index_ref_with_split(git_dir, format, &index, false)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

pub(crate) fn smudge_racily_clean_entries_before_write(
    git_dir: &Path,
    format: ObjectFormat,
    index: &mut Index,
) -> Result<()> {
    for position in racily_clean_entry_indexes_before_write(git_dir, format, index)? {
        index.entries[position].size = 0;
    }
    Ok(())
}

pub(crate) fn racily_clean_entry_indexes_before_write(
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
) -> Result<Vec<usize>> {
    let index_path = repository_index_path(git_dir);
    let Some(index_mtime) = fs::metadata(&index_path)
        .ok()
        .and_then(|metadata| sley_index::file_mtime_parts(&metadata))
    else {
        return Ok(Vec::new());
    };
    if index_mtime == (0, 0) {
        return Ok(Vec::new());
    }
    let Some(worktree_root) = (match worktree_root_for_git_dir(git_dir) {
        Ok(worktree_root) => worktree_root,
        Err(_) => return Ok(Vec::new()),
    }) else {
        return Ok(Vec::new());
    };
    let mut smudged = Vec::new();
    for (position, entry) in index.entries.iter().enumerate() {
        if index_entry_stage(entry) != 0 || sley_index::is_gitlink(entry.mode) {
            continue;
        }
        let entry_mtime = (
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        );
        if entry_mtime == (0, 0) || index_mtime > entry_mtime {
            continue;
        }
        let absolute = worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?);
        let Ok(metadata) = fs::symlink_metadata(&absolute) else {
            continue;
        };
        if entry.mode != worktree_entry_mode(&metadata)
            || !worktree_entry_is_uptodate(entry, &metadata)
        {
            continue;
        }
        let body = if metadata.file_type().is_symlink() {
            symlink_target_bytes(&absolute)?
        } else if metadata.is_file() {
            fs::read(&absolute)?
        } else {
            continue;
        };
        let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
        if oid != entry.oid {
            smudged.push(position);
        }
    }
    Ok(smudged)
}

pub(crate) fn invalidate_untracked_cache_for_git_paths(
    index: &mut Index,
    format: ObjectFormat,
    paths: &[Vec<u8>],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let Some(mut cache) = index.untracked_cache(format)? else {
        return Ok(());
    };
    let Some(root) = cache.root.as_mut() else {
        return Ok(());
    };
    for path in paths {
        invalidate_untracked_cache_dir_for_path(root, path);
    }
    index.set_untracked_cache(format, Some(&cache))
}

/// Carry index extensions whose semantics survive a checkout index rebuild.
///
/// `TREE`, `REUC`, split-index, sparse-index, fsmonitor, and layout extensions
/// describe the old entry table or its serialization and must be rebuilt or
/// discarded. The untracked-directory cache is independent of entry layout,
/// so Git preserves it while invalidating the directory nodes touched by paths
/// whose logical index state changed.
pub fn preserve_checkout_index_extensions(
    previous: &Index,
    next: &mut Index,
    format: ObjectFormat,
) -> Result<()> {
    let Some(cache) = previous.untracked_cache(format)? else {
        return Ok(());
    };
    next.set_untracked_cache(format, Some(&cache))?;

    type Fingerprint = (Stage, u32, ObjectId, bool, bool);
    fn entries_by_path(index: &Index) -> BTreeMap<Vec<u8>, Vec<Fingerprint>> {
        let mut entries = BTreeMap::<Vec<u8>, Vec<Fingerprint>>::new();
        for entry in &index.entries {
            entries
                .entry(entry.path.as_bytes().to_vec())
                .or_default()
                .push((
                    entry.stage(),
                    entry.mode,
                    entry.oid,
                    entry.is_skip_worktree(),
                    entry.is_intent_to_add(),
                ));
        }
        entries
    }

    let previous_entries = entries_by_path(previous);
    let next_entries = entries_by_path(next);
    let mut paths = previous_entries
        .keys()
        .chain(next_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    paths.retain(|path| previous_entries.get(path) != next_entries.get(path));
    invalidate_untracked_cache_for_git_paths(next, format, &paths.into_iter().collect::<Vec<_>>())
}

pub(crate) fn invalidate_untracked_cache_dir_for_path(root: &mut UntrackedCacheDir, path: &[u8]) {
    invalidate_untracked_cache_node(root);
    let mut current = root;
    let mut components = path.split(|byte| *byte == b'/').peekable();
    while let Some(component) = components.next() {
        if component.is_empty() || components.peek().is_none() {
            break;
        }
        let Some(child) = current.dirs.iter_mut().find(|dir| dir.name == component) else {
            break;
        };
        invalidate_untracked_cache_node(child);
        current = child;
    }
}

pub(crate) fn invalidate_untracked_cache_node(node: &mut UntrackedCacheDir) {
    node.valid = false;
    node.untracked.clear();
}

pub fn update_index_cacheinfo(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    entries: &[CacheInfoEntry],
    add: bool,
    verbose: bool,
) -> Result<UpdateIndexResult> {
    update_index_cacheinfo_with_options(
        git_dir,
        format,
        entries,
        UpdateIndexCacheInfoOptions {
            add,
            replace: false,
            verbose,
        },
    )
}

pub fn update_index_cacheinfo_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    entries: &[CacheInfoEntry],
    options: UpdateIndexCacheInfoOptions,
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        let db = FileObjectDatabase::from_git_dir(git_dir, format);
        expand_sparse_index_directories(&mut index, &db, format, |directory| {
            entries
                .iter()
                .any(|entry| entry.path.starts_with(directory))
        })?;
    }
    let mut updated = Vec::new();
    let mut reports: Vec<String> = Vec::new();
    let mut untracked_cache_invalidation_paths = Vec::new();
    for cacheinfo in entries {
        if !options.add
            && !index
                .entries
                .iter()
                .any(|existing| existing.path == cacheinfo.path)
        {
            let path = String::from_utf8_lossy(&cacheinfo.path);
            eprintln!("error: {path}: cannot add to the index - missing --add option?");
            eprintln!("fatal: git update-index: --cacheinfo cannot add {path}");
            return Err(GitError::Exit(128));
        }
        if !options.replace
            && index.entries.iter().any(|existing| {
                index_paths_have_directory_file_conflict(existing.path.as_bytes(), &cacheinfo.path)
            })
        {
            let path = String::from_utf8_lossy(&cacheinfo.path);
            eprintln!("error: '{path}' appears as both a file and as a directory");
            eprintln!("error: {path}: cannot add to the index - missing --add option?");
            eprintln!("fatal: git update-index: --cacheinfo cannot add {path}");
            return Err(GitError::Exit(128));
        }
        let flags = index_flags(cacheinfo.path.len(), cacheinfo.stage);
        let entry = IndexEntry {
            ctime_seconds: 0,
            ctime_nanoseconds: 0,
            mtime_seconds: 0,
            mtime_nanoseconds: 0,
            dev: 0,
            ino: 0,
            mode: cacheinfo.mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid: cacheinfo.oid,
            flags,
            flags_extended: 0,
            path: BString::from(cacheinfo.path.as_slice()),
        };
        if options.replace {
            remove_index_entries_under_dir(&mut index.entries, &cacheinfo.path);
            remove_index_dir_name_conflicts(&mut index.entries, &cacheinfo.path);
        }
        index.entries.retain(|existing| {
            existing.path != cacheinfo.path || index_entry_stage(existing) != cacheinfo.stage
        });
        index.entries.push(entry);
        // A following `--cacheinfo` record in the same invocation may need the
        // D/F replacement helpers, whose binary searches require index order.
        index.entries.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
        });
        untracked_cache_invalidation_paths.push(cacheinfo.path.clone());
        updated.push(cacheinfo.oid);
        // git's add_cacheinfo() calls report("add '%s'") *after* the entry is
        // staged, regardless of whether the subsequent index write succeeds.
        reports.push(format!(
            "add '{}'",
            String::from_utf8_lossy(&cacheinfo.path)
        ));
    }
    // git refuses to write an index entry whose object id is the null oid:
    // do_write_index() emits `error: cache entry has null sha1: <path>` and
    // returns nonzero, leaving the on-disk index untouched. The verbose `add`
    // line has already been printed by then.
    let null_entry = index.entries.iter().find(|entry| entry.oid.is_null());
    if let Some(entry) = null_entry {
        if options.verbose {
            flush_update_index_reports(&reports)?;
        }
        eprintln!(
            "error: cache entry has null sha1: {}",
            String::from_utf8_lossy(&entry.path)
        );
        return Err(GitError::Exit(128));
    }
    if !untracked_cache_invalidation_paths.is_empty() {
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
    }
    invalidate_untracked_cache_for_git_paths(
        &mut index,
        format,
        &untracked_cache_invalidation_paths,
    )?;
    write_repository_index_ref(git_dir, format, &index)?;
    if options.verbose {
        flush_update_index_reports(&reports)?;
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

fn index_paths_have_directory_file_conflict(left: &[u8], right: &[u8]) -> bool {
    fn is_directory_prefix(prefix: &[u8], path: &[u8]) -> bool {
        path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.get(prefix.len()) == Some(&b'/')
    }

    is_directory_prefix(left, right) || is_directory_prefix(right, left)
}

pub(crate) fn flush_update_index_reports(reports: &[String]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    for line in reports {
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
    Ok(())
}

pub fn update_index_index_info(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    records: &[IndexInfoRecord],
) -> Result<UpdateIndexResult> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        }
    };
    let mut updated = Vec::new();
    let mut untracked_cache_invalidation_paths = Vec::new();
    for record in records {
        match record {
            IndexInfoRecord::Remove { path } => {
                index.entries.retain(|existing| existing.path != *path);
                untracked_cache_invalidation_paths.push(path.clone());
            }
            IndexInfoRecord::Add(cacheinfo) => {
                let flags = index_flags(cacheinfo.path.len(), cacheinfo.stage);
                let entry = IndexEntry {
                    ctime_seconds: 0,
                    ctime_nanoseconds: 0,
                    mtime_seconds: 0,
                    mtime_nanoseconds: 0,
                    dev: 0,
                    ino: 0,
                    mode: cacheinfo.mode,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    oid: cacheinfo.oid,
                    flags,
                    flags_extended: 0,
                    path: BString::from(cacheinfo.path.as_slice()),
                };
                if cacheinfo.stage == 0 {
                    index
                        .entries
                        .retain(|existing| existing.path != cacheinfo.path);
                } else {
                    index.entries.retain(|existing| {
                        existing.path != cacheinfo.path
                            || index_entry_stage(existing) != cacheinfo.stage
                    });
                }
                index.entries.push(entry);
                untracked_cache_invalidation_paths.push(cacheinfo.path.clone());
                updated.push(cacheinfo.oid);
            }
        }
    }
    index.entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    if !untracked_cache_invalidation_paths.is_empty() {
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
    }
    invalidate_untracked_cache_for_git_paths(
        &mut index,
        format,
        &untracked_cache_invalidation_paths,
    )?;
    write_repository_index_ref(git_dir, format, &index)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

pub(crate) fn index_flags(path_len: usize, stage: u16) -> u16 {
    ((stage & 0x3) << 12) | ((path_len.min(0xfff) as u16) & 0x0fff)
}

pub(crate) const INDEX_FLAG_ASSUME_UNCHANGED: u16 = 0x8000;
pub(crate) const INDEX_FLAG_EXTENDED: u16 = 0x4000;
pub(crate) const INDEX_EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;

pub(crate) fn normalize_index_version_for_extended_flags(index: &mut Index) {
    let has_extended_flags = index
        .entries
        .iter()
        .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0);
    if has_extended_flags && index.version < 3 {
        index.version = 3;
    } else if !has_extended_flags && index.version == 3 {
        index.version = 2;
    }
}

pub(crate) fn index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// The oid of the stage-0 entry in `range` (the path's currently-tracked blob),
/// if any. Used by the safecrlf check to fetch `has_crlf_in_index`.
pub(crate) fn stage0_oid_in_range(
    entries: &[IndexEntry],
    range: std::ops::Range<usize>,
) -> Option<ObjectId> {
    entries[range]
        .iter()
        .find(|entry| index_entry_stage(entry) == 0)
        .map(|entry| entry.oid)
}

pub(crate) fn index_entry_skip_worktree(entry: &IndexEntry) -> bool {
    entry.flags & INDEX_FLAG_EXTENDED != 0
        && entry.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
}

pub(crate) fn print_update_index_path_error(path: &[u8], message: &str) {
    let path = String::from_utf8_lossy(path);
    eprintln!("error: {path}: {message}");
    eprintln!("fatal: Unable to process path {path}");
}

pub(crate) fn print_update_index_needs_update(path: &[u8]) {
    let path = String::from_utf8_lossy(path);
    println!("{path}: needs update");
}

pub fn write_tree_from_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<ObjectId> {
    write_tree_from_index_with_options(git_dir, format, WriteTreeOptions::default())
}

pub fn write_tree_from_index_with_odb(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    odb: &FileObjectDatabase,
) -> Result<ObjectId> {
    write_tree_from_index_with_options_and_odb(
        git_dir.as_ref(),
        format,
        WriteTreeOptions::default(),
        odb,
    )
}

pub fn write_tree_from_index_with_options(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: WriteTreeOptions,
) -> Result<ObjectId> {
    let git_dir = git_dir.as_ref();
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
    write_tree_from_index_with_options_and_odb(git_dir, format, options, &odb)
}

pub(crate) fn write_tree_from_index_with_options_and_odb(
    git_dir: &Path,
    format: ObjectFormat,
    options: WriteTreeOptions,
    odb: &FileObjectDatabase,
) -> Result<ObjectId> {
    let index_path = repository_index_path(git_dir);
    // A repository with no index file yet (fresh init, nothing staged) is an
    // empty index: `git write-tree` / `git commit --allow-empty` produce the
    // empty tree rather than erroring.
    let index_bytes = match fs::read(&index_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut checker = odb.presence_checker();
            let empty: &[WriteTreeEntry<'_>] = &[];
            return write_tree_entries_stream(
                empty,
                b"",
                None,
                odb,
                &mut checker,
                options.missing_ok,
            );
        }
        Err(err) => return Err(err.into()),
    };
    let mut checker = odb.presence_checker();
    let oid = if Index::bytes_have_extension(&index_bytes, format, b"link")? {
        let index = sley_index::read_repository_index(git_dir, format)?;
        write_tree_from_owned_index(&index, format, &options, odb, &mut checker)?
    } else {
        match BorrowedIndex::parse(&index_bytes, format) {
            Ok(index) => {
                write_tree_from_borrowed_index(&index, format, &options, odb, &mut checker)
            }
            Err(GitError::Unsupported(_)) => {
                let index = Index::parse(&index_bytes, format)?;
                write_tree_from_owned_index(&index, format, &options, odb, &mut checker)
            }
            Err(err) => Err(err),
        }?
    };
    if options.prefix.is_none() {
        refresh_repository_cache_tree(git_dir, format, odb)?;
    }
    Ok(oid)
}

pub(crate) fn write_tree_from_borrowed_index(
    index: &BorrowedIndex<'_>,
    format: ObjectFormat,
    options: &WriteTreeOptions,
    odb: &FileObjectDatabase,
    checker: &mut ObjectPresenceChecker,
) -> Result<ObjectId> {
    let cache_tree = if options.prefix.is_none() {
        index.cache_tree(format).ok().flatten()
    } else {
        None
    };
    if options.prefix.is_none() && !index.entries.iter().any(|entry| entry.is_intent_to_add()) {
        return write_tree_entries_stream(
            &index.entries,
            b"",
            cache_tree.as_ref(),
            odb,
            checker,
            options.missing_ok,
        );
    }
    // intent-to-add entries (`git add -N`, `git reset -N`) are placeholders that do
    // NOT belong in a written tree — git's cache_tree_update skips CE_INTENT_TO_ADD.
    // Drop them before building, so `write-tree` succeeds and the tree omits them
    // (their empty-blob oid is also typically absent from the odb).
    let entries = write_tree_entries_for_prefix(
        index
            .entries
            .iter()
            .filter(|entry| !entry.is_intent_to_add()),
        options.prefix.as_deref(),
    )?;
    write_tree_entries_stream(
        &entries,
        b"",
        cache_tree.as_ref(),
        odb,
        checker,
        options.missing_ok,
    )
}

pub(crate) fn write_tree_from_owned_index(
    index: &Index,
    format: ObjectFormat,
    options: &WriteTreeOptions,
    odb: &FileObjectDatabase,
    checker: &mut ObjectPresenceChecker,
) -> Result<ObjectId> {
    let cache_tree = if options.prefix.is_none() {
        index.cache_tree(format).ok().flatten()
    } else {
        None
    };
    if options.prefix.is_none() && !index.entries.iter().any(|entry| entry.is_intent_to_add()) {
        return write_tree_entries_stream(
            &index.entries,
            b"",
            cache_tree.as_ref(),
            odb,
            checker,
            options.missing_ok,
        );
    }
    let entries = write_tree_entries_for_prefix(
        index
            .entries
            .iter()
            .filter(|entry| !entry.is_intent_to_add()),
        options.prefix.as_deref(),
    )?;
    write_tree_entries_stream(
        &entries,
        b"",
        cache_tree.as_ref(),
        odb,
        checker,
        options.missing_ok,
    )
}

#[derive(Clone, Copy)]
pub(crate) struct WriteTreeEntry<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) mode: u32,
    pub(crate) oid: ObjectId,
}

pub(crate) trait WriteTreeIndexEntry {
    fn write_tree_path(&self) -> &[u8];
    fn write_tree_mode(&self) -> u32;
    fn write_tree_oid(&self) -> ObjectId;
}

impl WriteTreeIndexEntry for IndexEntry {
    fn write_tree_path(&self) -> &[u8] {
        self.path.as_bytes()
    }

    fn write_tree_mode(&self) -> u32 {
        self.mode
    }

    fn write_tree_oid(&self) -> ObjectId {
        self.oid
    }
}

impl WriteTreeIndexEntry for IndexEntryRef<'_> {
    fn write_tree_path(&self) -> &[u8] {
        self.path
    }

    fn write_tree_mode(&self) -> u32 {
        self.mode
    }

    fn write_tree_oid(&self) -> ObjectId {
        self.oid
    }
}

impl WriteTreeIndexEntry for WriteTreeEntry<'_> {
    fn write_tree_path(&self) -> &[u8] {
        self.path
    }

    fn write_tree_mode(&self) -> u32 {
        self.mode
    }

    fn write_tree_oid(&self) -> ObjectId {
        self.oid
    }
}

pub(crate) fn write_tree_entries_for_prefix<'a, E>(
    entries: impl IntoIterator<Item = &'a E>,
    prefix: Option<&[u8]>,
) -> Result<Vec<WriteTreeEntry<'a>>>
where
    E: WriteTreeIndexEntry + 'a,
{
    let Some(prefix) = prefix else {
        return Ok(entries
            .into_iter()
            .map(|entry| WriteTreeEntry {
                path: entry.write_tree_path(),
                mode: entry.write_tree_mode(),
                oid: entry.write_tree_oid(),
            })
            .collect());
    };
    let trimmed_len = prefix
        .iter()
        .rposition(|byte| *byte != b'/')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let trimmed = &prefix[..trimmed_len];
    if trimmed.is_empty() {
        return Ok(entries
            .into_iter()
            .map(|entry| WriteTreeEntry {
                path: entry.write_tree_path(),
                mode: entry.write_tree_mode(),
                oid: entry.write_tree_oid(),
            })
            .collect());
    }
    let mut prefixed = Vec::new();
    for entry in entries {
        let Some(remainder) = entry.write_tree_path().strip_prefix(trimmed) else {
            continue;
        };
        let Some(stripped) = remainder.strip_prefix(b"/") else {
            continue;
        };
        if stripped.is_empty() {
            continue;
        }
        prefixed.push(WriteTreeEntry {
            path: stripped,
            mode: entry.write_tree_mode(),
            oid: entry.write_tree_oid(),
        });
    }
    if prefixed.is_empty() {
        eprintln!(
            "fatal: git-write-tree: prefix {} not found",
            String::from_utf8_lossy(prefix)
        );
        return Err(GitError::Exit(128));
    }
    Ok(prefixed)
}

pub(crate) fn write_tree_entries_stream<E>(
    entries: &[E],
    prefix: &[u8],
    cache_tree: Option<&CacheTree>,
    odb: &FileObjectDatabase,
    checker: &mut ObjectPresenceChecker,
    missing_ok: bool,
) -> Result<ObjectId>
where
    E: WriteTreeIndexEntry,
{
    if let Some(oid) = valid_cache_tree_oid(cache_tree, entries.len()) {
        return Ok(oid);
    }

    let mut tree_entries = Vec::new();
    let mut index = 0usize;
    while index < entries.len() {
        let entry = &entries[index];
        let path = entry.write_tree_path();
        let Some(remainder) = path.strip_prefix(prefix) else {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        };
        if remainder.is_empty() || remainder[0] == b'/' {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        }

        if entry.write_tree_mode() == SPARSE_DIR_MODE
            && let Some(name) = remainder.strip_suffix(b"/")
            && !name.is_empty()
            && !name.contains(&b'/')
        {
            let oid = entry.write_tree_oid();
            if !missing_ok && !checker.contains(&oid)? {
                eprintln!(
                    "error: invalid object {:o} {} for '{}'",
                    SPARSE_DIR_MODE,
                    oid,
                    String::from_utf8_lossy(path)
                );
                eprintln!("fatal: git-write-tree: error building trees");
                return Err(GitError::Exit(128));
            }
            tree_entries.push(TreeEntry {
                mode: SPARSE_DIR_MODE,
                name: BString::from(name),
                oid,
            });
            index += 1;
            continue;
        }

        if let Some(slash) = remainder.iter().position(|byte| *byte == b'/') {
            let name = &remainder[..slash];
            if name.is_empty() {
                return Err(GitError::InvalidPath(format!(
                    "invalid index path {}",
                    String::from_utf8_lossy(path)
                )));
            }
            let start = index;
            let child_cache = cache_tree.and_then(|tree| {
                tree.subtrees
                    .iter()
                    .find(|child| child.name.as_slice() == name)
                    .map(|child| &child.tree)
            });
            if let Some(cached_count) = valid_cache_tree_entry_count(child_cache) {
                let end = start.saturating_add(cached_count);
                if cached_count > 0
                    && end <= entries.len()
                    && same_tree_component(entries[end - 1].write_tree_path(), prefix, name)?
                    && (end == entries.len()
                        || !same_tree_component(entries[end].write_tree_path(), prefix, name)?)
                {
                    index = end;
                } else {
                    index += 1;
                    while index < entries.len()
                        && same_tree_component(entries[index].write_tree_path(), prefix, name)?
                    {
                        index += 1;
                    }
                }
            } else {
                index += 1;
                while index < entries.len()
                    && same_tree_component(entries[index].write_tree_path(), prefix, name)?
                {
                    index += 1;
                }
            }
            if let Some(oid) = valid_cache_tree_oid(child_cache, index - start) {
                tree_entries.push(TreeEntry {
                    mode: 0o040000,
                    name: BString::from(name),
                    oid,
                });
                continue;
            }
            let mut child_prefix = Vec::with_capacity(prefix.len() + name.len() + 1);
            child_prefix.extend_from_slice(prefix);
            child_prefix.extend_from_slice(name);
            child_prefix.push(b'/');
            let oid = write_tree_entries_stream(
                &entries[start..index],
                &child_prefix,
                child_cache,
                odb,
                checker,
                missing_ok,
            )?;
            tree_entries.push(TreeEntry {
                mode: 0o040000,
                name: BString::from(name),
                oid,
            });
            continue;
        }

        let mode = entry.write_tree_mode();
        let oid = entry.write_tree_oid();
        if !missing_ok && !sley_index::is_gitlink(mode) && !checker.contains(&oid)? {
            eprintln!(
                "error: invalid object {:o} {} for '{}'",
                mode,
                oid,
                String::from_utf8_lossy(path)
            );
            eprintln!("fatal: git-write-tree: error building trees");
            return Err(GitError::Exit(128));
        }
        tree_entries.push(TreeEntry {
            mode,
            name: BString::from(remainder),
            oid,
        });
        index += 1;
    }

    tree_entries.sort_by(|left, right| {
        git_tree_entry_cmp(
            left.name.as_bytes(),
            left.mode,
            right.name.as_bytes(),
            right.mode,
        )
    });
    odb.write_object(EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: tree_entries,
        }
        .write(),
    ))
}

pub(crate) fn index_entry_tree_oid_without_cache(
    index: &Index,
    format: ObjectFormat,
) -> Result<ObjectId> {
    let entries = write_tree_entries_for_prefix(
        index
            .entries
            .iter()
            .filter(|entry| entry.stage() == Stage::Normal && !entry.is_intent_to_add()),
        None,
    )?;
    write_tree_entries_oid_without_cache(&entries, b"", format)
}

pub(crate) fn borrowed_index_entry_tree_oid_without_cache(
    index: &BorrowedIndex<'_>,
    format: ObjectFormat,
) -> Result<ObjectId> {
    let entries = write_tree_entries_for_prefix(
        index
            .entries
            .iter()
            .filter(|entry| entry.stage() == Stage::Normal && !entry.is_intent_to_add()),
        None,
    )?;
    write_tree_entries_oid_without_cache(&entries, b"", format)
}

pub(crate) fn write_tree_entries_oid_without_cache<E>(
    entries: &[E],
    prefix: &[u8],
    format: ObjectFormat,
) -> Result<ObjectId>
where
    E: WriteTreeIndexEntry,
{
    let mut tree_entries = Vec::new();
    let mut index = 0usize;
    while index < entries.len() {
        let entry = &entries[index];
        let path = entry.write_tree_path();
        let Some(remainder) = path.strip_prefix(prefix) else {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        };
        if remainder.is_empty() || remainder[0] == b'/' {
            return Err(GitError::InvalidPath(format!(
                "invalid index path {}",
                String::from_utf8_lossy(path)
            )));
        }

        if entry.write_tree_mode() == SPARSE_DIR_MODE
            && let Some(name) = remainder.strip_suffix(b"/")
            && !name.is_empty()
            && !name.contains(&b'/')
        {
            tree_entries.push(TreeEntry {
                mode: SPARSE_DIR_MODE,
                name: BString::from(name),
                oid: entry.write_tree_oid(),
            });
            index += 1;
            continue;
        }

        if let Some(slash) = remainder.iter().position(|byte| *byte == b'/') {
            let name = &remainder[..slash];
            if name.is_empty() {
                return Err(GitError::InvalidPath(format!(
                    "invalid index path {}",
                    String::from_utf8_lossy(path)
                )));
            }
            let start = index;
            index += 1;
            while index < entries.len()
                && same_tree_component(entries[index].write_tree_path(), prefix, name)?
            {
                index += 1;
            }
            let mut child_prefix = Vec::with_capacity(prefix.len() + name.len() + 1);
            child_prefix.extend_from_slice(prefix);
            child_prefix.extend_from_slice(name);
            child_prefix.push(b'/');
            let oid = write_tree_entries_oid_without_cache(
                &entries[start..index],
                &child_prefix,
                format,
            )?;
            tree_entries.push(TreeEntry {
                mode: 0o040000,
                name: BString::from(name),
                oid,
            });
            continue;
        }

        tree_entries.push(TreeEntry {
            mode: entry.write_tree_mode(),
            name: BString::from(remainder),
            oid: entry.write_tree_oid(),
        });
        index += 1;
    }

    tree_entries.sort_by(|left, right| {
        git_tree_entry_cmp(
            left.name.as_bytes(),
            left.mode,
            right.name.as_bytes(),
            right.mode,
        )
    });
    EncodedObject::new(
        ObjectType::Tree,
        Tree {
            entries: tree_entries,
        }
        .write(),
    )
    .object_id(format)
}

pub(crate) fn valid_cache_tree_oid(
    tree: Option<&CacheTree>,
    entry_count: usize,
) -> Option<ObjectId> {
    let tree = tree?;
    if valid_cache_tree_entry_count(Some(tree))? != entry_count {
        return None;
    }
    tree.oid
}

pub(crate) fn valid_cache_tree_entry_count(tree: Option<&CacheTree>) -> Option<usize> {
    let tree = tree?;
    if tree.entry_count < 0 || tree.oid.is_none() {
        return None;
    }
    Some(tree.entry_count as usize)
}

pub(crate) fn same_tree_component(path: &[u8], prefix: &[u8], name: &[u8]) -> Result<bool> {
    let Some(remainder) = path.strip_prefix(prefix) else {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
            String::from_utf8_lossy(path)
        )));
    };
    Ok(remainder.starts_with(name) && remainder.get(name.len()) == Some(&b'/'))
}
