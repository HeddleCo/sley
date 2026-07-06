//! The `git status` engine: short-status streaming/collection, tracked-only fast paths, and untracked-status rows.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::checkout::*;
use crate::ignore::*;
use crate::index::*;
use crate::index_io::*;
use crate::types_admin::*;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn stream_short_status<F>(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    emit: F,
) -> Result<()>
where
    F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
{
    stream_short_status_with_options(
        worktree_root,
        git_dir,
        format,
        ShortStatusOptions::default(),
        emit,
    )
}

pub fn short_status_count(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<usize> {
    short_status_count_with_options(
        worktree_root,
        git_dir,
        format,
        ShortStatusOptions::default(),
    )
}

pub fn short_status_count_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: ShortStatusOptions,
) -> Result<usize> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if !options.include_ignored
        && let Some(count) = short_status_borrowed_head_matches_index_count_if_possible(
            worktree_root,
            git_dir,
            format,
            &db,
            options.untracked_mode,
        )?
    {
        return Ok(count);
    }
    let mut count = 0usize;
    stream_short_status_with_options(worktree_root, git_dir, format, options, |_| {
        count += 1;
        Ok(StreamControl::Continue)
    })?;
    Ok(count)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StatusProfileCounters {
    pub(crate) fast_path_borrowed: bool,
    pub(crate) read_dir_calls: u64,
    pub(crate) dir_entries_seen: u64,
    pub(crate) file_type_calls: u64,
    pub(crate) ignore_checks: u64,
    pub(crate) ignore_pattern_tests: u64,
    pub(crate) ignore_glob_fallback_tests: u64,
    pub(crate) tracked_exact_hits: u64,
    pub(crate) tracked_dir_prefix_hits: u64,
    pub(crate) tracked_skip_worktree_prefix_hits: u64,
    pub(crate) read_dir_entry_vec_cap_bytes: u64,
    pub(crate) read_dir_entry_vec_max_len: u64,
    pub(crate) read_dir_entry_vec_max_cap: u64,
    pub(crate) read_dir_name_vec_cap_bytes: u64,
    pub(crate) read_dir_name_vec_max_len: u64,
    pub(crate) read_dir_name_vec_max_cap: u64,
    pub(crate) untracked_rows: u64,
    pub(crate) tracked_elapsed_us: u128,
    pub(crate) untracked_elapsed_us: u128,
    pub(crate) render_elapsed_us: u128,
    pub(crate) overlap_enabled: bool,
}

pub(crate) const STATUS_BORROWED_OVERLAP_MIN_STAGE0: usize = 8192;

#[derive(Debug, Clone, Copy)]
pub(crate) struct StatusExecutor {
    workers: usize,
}

impl StatusExecutor {
    pub(crate) fn new() -> Self {
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .max(1);
        Self { workers }
    }

    pub(crate) fn worker_count_for(
        self,
        item_count: usize,
        min_items_per_worker: usize,
        cap: usize,
    ) -> usize {
        if item_count == 0 {
            return 0;
        }
        let requested = item_count.div_ceil(min_items_per_worker.max(1));
        self.workers.min(cap.max(1)).min(requested).max(1)
    }

    pub(crate) fn spawn<'scope, 'env, F, T>(
        self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
        name: &str,
        f: F,
    ) -> Result<StatusTask<'scope, T>>
    where
        F: FnOnce() -> Result<T> + Send + 'scope,
        T: Send + 'scope,
    {
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn_scoped(scope, f)
            .map_err(|err| {
                GitError::Command(format!("failed to spawn status worker `{name}`: {err}"))
            })?;
        Ok(StatusTask {
            name: name.to_string(),
            handle,
        })
    }
}

pub(crate) struct StatusTask<'scope, T> {
    name: String,
    handle: std::thread::ScopedJoinHandle<'scope, Result<T>>,
}

impl<T> StatusTask<'_, T> {
    pub(crate) fn join(self) -> Result<T> {
        self.handle
            .join()
            .map_err(|_| GitError::Command(format!("status worker `{}` panicked", self.name)))?
    }
}

pub(crate) enum BorrowedIndexBytes {
    Owned(Vec<u8>),
    Mapped(sley_mmap::MappedFile),
}

impl AsRef<[u8]> for BorrowedIndexBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(bytes) => bytes.as_bytes(),
        }
    }
}

pub(crate) fn read_borrowed_index_bytes(index_path: &Path) -> Result<BorrowedIndexBytes> {
    match sley_mmap::MappedFile::open_index(index_path) {
        Ok(mapped) => Ok(BorrowedIndexBytes::Mapped(mapped)),
        Err(_) => Ok(BorrowedIndexBytes::Owned(fs::read(index_path)?)),
    }
}

impl StatusProfileCounters {
    fn enabled() -> bool {
        std::env::var_os("SLEY_STATUS_PROFILE").is_some_and(|value| value != "0")
    }

    fn memory_enabled() -> bool {
        std::env::var_os("SLEY_STATUS_PROFILE")
            .and_then(|value| value.into_string().ok())
            .is_some_and(|value| value == "mem" || value == "memory")
    }

    pub(crate) fn merge_untracked(&mut self, other: StatusProfileCounters) {
        self.read_dir_calls += other.read_dir_calls;
        self.dir_entries_seen += other.dir_entries_seen;
        self.file_type_calls += other.file_type_calls;
        self.ignore_checks += other.ignore_checks;
        self.ignore_pattern_tests += other.ignore_pattern_tests;
        self.ignore_glob_fallback_tests += other.ignore_glob_fallback_tests;
        self.tracked_exact_hits += other.tracked_exact_hits;
        self.tracked_dir_prefix_hits += other.tracked_dir_prefix_hits;
        self.tracked_skip_worktree_prefix_hits += other.tracked_skip_worktree_prefix_hits;
        self.read_dir_entry_vec_cap_bytes += other.read_dir_entry_vec_cap_bytes;
        self.read_dir_entry_vec_max_len = self
            .read_dir_entry_vec_max_len
            .max(other.read_dir_entry_vec_max_len);
        self.read_dir_entry_vec_max_cap = self
            .read_dir_entry_vec_max_cap
            .max(other.read_dir_entry_vec_max_cap);
        self.read_dir_name_vec_cap_bytes += other.read_dir_name_vec_cap_bytes;
        self.read_dir_name_vec_max_len = self
            .read_dir_name_vec_max_len
            .max(other.read_dir_name_vec_max_len);
        self.read_dir_name_vec_max_cap = self
            .read_dir_name_vec_max_cap
            .max(other.read_dir_name_vec_max_cap);
        self.untracked_rows += other.untracked_rows;
        self.untracked_elapsed_us += other.untracked_elapsed_us;
    }

    fn emit(&self) {
        eprintln!(
            "{{\"schema\":\"sley.status.profile.v1\",\
             \"fast_path_borrowed\":{},\
             \"read_dir_calls\":{},\
             \"dir_entries_seen\":{},\
             \"file_type_calls\":{},\
             \"ignore_checks\":{},\
             \"ignore_pattern_tests\":{},\
             \"ignore_glob_fallback_tests\":{},\
             \"tracked_exact_hits\":{},\
             \"tracked_dir_prefix_hits\":{},\
             \"tracked_skip_worktree_prefix_hits\":{},\
             \"read_dir_entry_size\":{},\
             \"read_dir_entry_vec_cap_bytes\":{},\
             \"read_dir_entry_vec_max_len\":{},\
             \"read_dir_entry_vec_max_cap\":{},\
             \"read_dir_name_size\":{},\
             \"read_dir_name_vec_cap_bytes\":{},\
             \"read_dir_name_vec_max_len\":{},\
             \"read_dir_name_vec_max_cap\":{},\
             \"untracked_rows\":{},\
             \"tracked_elapsed_us\":{},\
             \"untracked_elapsed_us\":{},\
             \"render_elapsed_us\":{},\
             \"overlap_enabled\":{}}}",
            self.fast_path_borrowed,
            self.read_dir_calls,
            self.dir_entries_seen,
            self.file_type_calls,
            self.ignore_checks,
            self.ignore_pattern_tests,
            self.ignore_glob_fallback_tests,
            self.tracked_exact_hits,
            self.tracked_dir_prefix_hits,
            self.tracked_skip_worktree_prefix_hits,
            std::mem::size_of::<StatusDirEntry>(),
            self.read_dir_entry_vec_cap_bytes,
            self.read_dir_entry_vec_max_len,
            self.read_dir_entry_vec_max_cap,
            std::mem::size_of::<std::ffi::OsString>(),
            self.read_dir_name_vec_cap_bytes,
            self.read_dir_name_vec_max_len,
            self.read_dir_name_vec_max_cap,
            self.untracked_rows,
            self.tracked_elapsed_us,
            self.untracked_elapsed_us,
            self.render_elapsed_us,
            self.overlap_enabled
        );
    }
}

pub(crate) fn status_profile_rss_vsz_bytes() -> Option<(u64, u64)> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-o", "vsz=", "-p", &pid])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut parts = text.split_whitespace();
    let rss_kib = parts.next()?.parse::<u64>().ok()?;
    let vsz_kib = parts.next()?.parse::<u64>().ok()?;
    Some((rss_kib * 1024, vsz_kib * 1024))
}

pub(crate) fn status_profile_pause(label: &str) {
    let Some(target) =
        std::env::var_os("SLEY_STATUS_PROFILE_PAUSE_AT").and_then(|value| value.into_string().ok())
    else {
        return;
    };
    if target != label && target != "*" {
        return;
    }
    let seconds = std::env::var("SLEY_STATUS_PROFILE_PAUSE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30);
    eprintln!(
        "{{\"schema\":\"sley.status.mem.pause.v1\",\"label\":\"{}\",\"pid\":{},\"seconds\":{}}}",
        label,
        std::process::id(),
        seconds
    );
    std::thread::sleep(std::time::Duration::from_secs(seconds));
}

pub(crate) fn status_profile_mem(label: &str, details: &[(&str, usize)]) {
    if !StatusProfileCounters::memory_enabled() {
        return;
    }
    let (rss_bytes, vsz_bytes) = status_profile_rss_vsz_bytes().unwrap_or((0, 0));
    eprint!(
        "{{\"schema\":\"sley.status.mem.v1\",\"label\":\"{}\",\"pid\":{},\"rss_bytes\":{},\"vsz_bytes\":{}",
        label,
        std::process::id(),
        rss_bytes,
        vsz_bytes
    );
    for (key, value) in details {
        eprint!(",\"{}\":{}", key, value);
    }
    eprintln!("}}");
    status_profile_pause(label);
}

/// Compare one expected tracked entry to the worktree path named by `path`.
///
/// `path` is repository-relative and uses the platform path representation. For
/// callers that already carry git's byte path form, use
/// [`worktree_entry_state_by_git_path`].
pub fn worktree_entry_state(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    path: impl AsRef<Path>,
    expected_oid: &ObjectId,
    expected_mode: u32,
    index_probe: Option<&IndexStatProbe>,
) -> Result<WorktreeEntryState> {
    let path = path.as_ref();
    if path.is_absolute() {
        return Err(GitError::InvalidPath(format!(
            "worktree entry path {} is absolute",
            path.display()
        )));
    }
    let git_path = git_path_bytes(path)?;
    worktree_entry_state_by_git_path(
        worktree_root,
        git_dir,
        format,
        &git_path,
        expected_oid,
        expected_mode,
        index_probe,
    )
}

/// Compare one expected tracked entry to the worktree path named by a
/// repository-relative git path (`/` separators, raw bytes).
///
/// The comparison uses the same clean-filter, symlink-target, gitlink, and
/// racy-clean stat shortcut rules as [`stream_short_status_with_options`].
pub fn worktree_entry_state_by_git_path(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    git_path: &[u8],
    expected_oid: &ObjectId,
    expected_mode: u32,
    index_probe: Option<&IndexStatProbe>,
) -> Result<WorktreeEntryState> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let stat_cache =
        index_probe.and_then(|probe| probe.stat_cache_for(git_path, expected_oid, expected_mode));
    let Some(worktree_entry) = worktree_entry_for_git_path(
        worktree_root,
        git_dir,
        format,
        git_path,
        expected_oid,
        expected_mode,
        stat_cache.as_ref(),
    )?
    else {
        return Ok(WorktreeEntryState::Deleted);
    };
    if worktree_entry.mode == expected_mode && worktree_entry.oid == *expected_oid {
        Ok(WorktreeEntryState::Clean)
    } else {
        Ok(WorktreeEntryState::Modified)
    }
}

pub fn stream_short_status_with_options<F>(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: ShortStatusOptions,
    mut emit: F,
) -> Result<()>
where
    F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
{
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if !options.include_ignored
        && let Some(()) = stream_short_status_borrowed_head_matches_index_if_possible(
            worktree_root,
            git_dir,
            format,
            &db,
            options.untracked_mode,
            &mut emit,
        )?
    {
        return Ok(());
    }
    for entry in collect_short_status_with_options(worktree_root, git_dir, format, options)? {
        if emit(entry.as_row())?.is_stop() {
            break;
        }
    }
    Ok(())
}

pub(crate) fn collect_short_status_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: ShortStatusOptions,
) -> Result<Vec<ShortStatusEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    if !options.include_ignored
        && let Some(entries) = short_status_borrowed_head_matches_index_if_possible(
            worktree_root,
            git_dir,
            format,
            &db,
            options.untracked_mode,
        )?
    {
        return Ok(entries);
    }
    // Parse the index once: the stat cache lets the worktree walk skip
    // re-hashing files whose stat proves they are unchanged since staging
    // (git's racy-git shortcut). When HEAD matches the index, the status
    // comparison can stream directly from the parsed index and avoid building a
    // second path-sorted copy of every tracked entry.
    let (mut parsed_index, mut stat_cache, mut head_matches_index) =
        read_index_with_stat_cache(git_dir, format, &db)?;
    let sparse_checkout_active = sparse_checkout_active_for_status(git_dir, &parsed_index);
    if sparse_checkout_active && parsed_index.entries.iter().any(IndexEntry::is_sparse_dir) {
        expand_sparse_index(&mut parsed_index, &db, format)?;
        stat_cache = IndexStatCache::from_index_mtime(&parsed_index, stat_cache.index_mtime);
        head_matches_index = false;
    }
    let mut unmerged_entries = short_status_unmerged_entries(&parsed_index);
    let unmerged_paths = unmerged_entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    if head_matches_index && !options.include_ignored {
        let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
        let entries = short_status_tracked_only(
            worktree_root,
            git_dir,
            format,
            &db,
            &parsed_index,
            &stat_cache,
            true,
            sparse_checkout_active,
            options.untracked_mode,
        );
        let mut entries = entries?;
        entries.retain(|entry| !unmerged_paths.contains(&entry.path));
        let untracked_paths = status_untracked_paths_from_index(
            worktree_root,
            git_dir,
            &parsed_index,
            &stat_cache,
            &mut ignores,
            options.untracked_mode,
            None,
        )?;
        for path in untracked_paths {
            entries.push(ShortStatusEntry {
                index: b'?',
                worktree: b'?',
                path,
                head_mode: None,
                index_mode: None,
                worktree_mode: None,
                head_oid: None,
                index_oid: None,
                submodule: None,
            });
        }
        entries.append(&mut unmerged_entries);
        entries.sort_by(|left, right| {
            status_sort_category(left)
                .cmp(&status_sort_category(right))
                .then_with(|| left.path.cmp(&right.path))
        });
        return Ok(entries);
    }
    let index = index_entries_from_index(parsed_index);
    let head = if head_matches_index {
        None
    } else {
        Some(head_tree_entries(git_dir, format, &db)?)
    };
    let known_tracked_paths = index.keys().cloned().collect::<BTreeSet<_>>();
    let tracked_paths = if options.untracked_mode == StatusUntrackedMode::None {
        Some(&known_tracked_paths)
    } else {
        None
    };
    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
    let (worktree, submodule_dirt_map, tracked_presence) =
        status_worktree_entries_with_submodule_dirt(
            worktree_root,
            git_dir,
            format,
            &stat_cache,
            Some(&known_tracked_paths),
            tracked_paths,
            Some(&mut ignores),
        )?;
    let mut entries = Vec::new();
    if head_matches_index {
        collect_status_entries_head_matches_index(
            &index,
            &worktree,
            &tracked_presence,
            &stat_cache,
            sparse_checkout_active,
            &submodule_dirt_map,
            options.untracked_mode,
            &mut entries,
        );
    } else if let Some(head) = head.as_ref() {
        collect_status_entries_with_head(
            StatusComparisonInputs {
                head,
                index: &index,
                worktree: &worktree,
                tracked_presence: &tracked_presence,
                stat_cache: &stat_cache,
                sparse_checkout_active,
                submodule_dirt_map: &submodule_dirt_map,
                ignores: &ignores,
            },
            options.untracked_mode,
            &mut entries,
        );
    }
    entries.retain(|entry| !unmerged_paths.contains(&entry.path));
    entries.append(&mut unmerged_entries);
    if options.include_ignored {
        let ignored_directory_rows = matches!(options.ignored_mode, StatusIgnoredMode::Matching)
            || !matches!(options.untracked_mode, StatusUntrackedMode::All);
        let ignored_paths = ignored_untracked_paths(
            worktree_root,
            git_dir,
            &index,
            &ignores,
            ignored_directory_rows,
        )?;
        let ignored_paths: Vec<Vec<u8>> = match options.ignored_mode {
            StatusIgnoredMode::Matching => ignored_paths,
            StatusIgnoredMode::Traditional
                if matches!(options.untracked_mode, StatusUntrackedMode::All) =>
            {
                ignored_paths
            }
            StatusIgnoredMode::Traditional => {
                let mut rolled = BTreeSet::new();
                for path in ignored_paths {
                    let path = ignored_traditional_rollup_path(
                        worktree_root,
                        git_dir,
                        &path,
                        &index,
                        &ignores,
                    )?;
                    if ignored_traditional_path_is_empty_directory(worktree_root, &path)? {
                        continue;
                    }
                    rolled.insert(path);
                }
                rolled.into_iter().collect()
            }
        };
        for path in ignored_paths {
            entries.push(ShortStatusEntry {
                index: b'!',
                worktree: b'!',
                path,
                head_mode: None,
                index_mode: None,
                worktree_mode: None,
                head_oid: None,
                index_oid: None,
                submodule: None,
            });
        }
    }
    let untracked_paths: Vec<Vec<u8>> = match options.untracked_mode {
        StatusUntrackedMode::All => worktree
            .iter()
            .filter_map(|(path, entry)| {
                let is_directory = entry.mode == 0o040000 && entry.oid.is_null();
                if index.contains_key(path)
                    || path_or_parent_is_ignored(&ignores, path, is_directory)
                {
                    return None;
                }
                if is_directory {
                    let mut directory = path.clone();
                    directory.push(b'/');
                    Some(directory)
                } else {
                    Some(path.clone())
                }
            })
            .collect(),
        StatusUntrackedMode::Normal => {
            normal_untracked_paths_from_worktree(&worktree, &index, &ignores)
        }
        StatusUntrackedMode::None => Vec::new(),
    };
    for path in untracked_paths {
        entries.push(ShortStatusEntry {
            index: b'?',
            worktree: b'?',
            path,
            head_mode: None,
            index_mode: None,
            worktree_mode: None,
            head_oid: None,
            index_oid: None,
            submodule: None,
        });
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn short_status_unmerged_entries(index: &Index) -> Vec<ShortStatusEntry> {
    let mut by_path: BTreeMap<Vec<u8>, BTreeSet<u16>> = BTreeMap::new();
    for entry in &index.entries {
        let stage = entry.stage().as_u16();
        if stage > 0 {
            by_path
                .entry(entry.path.as_bytes().to_vec())
                .or_default()
                .insert(stage);
        }
    }
    by_path
        .into_iter()
        .map(|(path, stages)| {
            let (index, worktree) = short_status_unmerged_codes(&stages);
            ShortStatusEntry {
                index,
                worktree,
                path,
                head_mode: None,
                index_mode: None,
                worktree_mode: None,
                head_oid: None,
                index_oid: None,
                submodule: None,
            }
        })
        .collect()
}

pub(crate) fn short_status_unmerged_codes(stages: &BTreeSet<u16>) -> (u8, u8) {
    match (
        stages.contains(&1),
        stages.contains(&2),
        stages.contains(&3),
    ) {
        (true, false, false) => (b'D', b'D'),
        (false, true, false) => (b'A', b'U'),
        (true, true, false) => (b'U', b'D'),
        (false, false, true) => (b'U', b'A'),
        (true, false, true) => (b'D', b'U'),
        (false, true, true) => (b'A', b'A'),
        (true, true, true) => (b'U', b'U'),
        (false, false, false) => (b'U', b'U'),
    }
}

pub(crate) fn sparse_checkout_active_for_status(git_dir: &Path, index: &Index) -> bool {
    index.is_sparse()
        || index.entries.iter().any(IndexEntry::is_sparse_dir)
        || sparse_checkout_config_enabled(git_dir)
}

pub(crate) fn sparse_checkout_active_for_borrowed_status(
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
) -> bool {
    index
        .entries
        .iter()
        .any(|entry| entry.mode == SPARSE_DIR_MODE && entry.is_skip_worktree())
        || sparse_checkout_config_enabled(git_dir)
}

pub(crate) fn sparse_checkout_config_enabled(git_dir: &Path) -> bool {
    GitConfig::read(git_dir.join("config"))
        .ok()
        .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
        == Some(true)
        || GitConfig::read(git_dir.join("config.worktree"))
            .ok()
            .and_then(|config| config.get_bool("core", None, "sparseCheckout"))
            == Some(true)
}

pub(crate) fn collect_status_entries_head_matches_index(
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    tracked_presence: &HashSet<Vec<u8>>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    submodule_dirt_map: &BTreeMap<Vec<u8>, u8>,
    untracked_mode: StatusUntrackedMode,
    entries: &mut Vec<ShortStatusEntry>,
) {
    for (path, index_entry) in index {
        let intent_to_add = stat_cache
            .index_entry(path)
            .is_some_and(IndexEntry::is_intent_to_add);
        let visible_index_entry = (!intent_to_add).then_some(index_entry);
        let worktree_entry = worktree.get(path);
        let worktree_present =
            worktree_entry.is_some() || tracked_presence.contains(path.as_slice());
        // Honor the skip-worktree bit literally unless sparse-checkout is active
        // and the file is present (git clears the bit then -> changes reported).
        let skip_worktree = stat_cache
            .index_entry(path)
            .is_some_and(index_entry_skip_worktree)
            && (!sparse_checkout_active || !worktree_present);
        let submodule = status_submodule_from_entries(
            path,
            index_entry,
            worktree_entry,
            submodule_dirt_map,
            untracked_mode,
        );
        let worktree_code = match worktree_entry {
            _ if skip_worktree => b' ',
            None if intent_to_add => b' ',
            None if !worktree_present => b'D',
            Some(_) if intent_to_add => b'A',
            Some(worktree_entry) if Some(worktree_entry) != visible_index_entry => {
                status_change_code(index_entry.mode, worktree_entry.mode)
            }
            _ if submodule.is_some_and(|sub| sub.any()) => b'M',
            _ => b' ',
        };
        if worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: b' ',
                worktree: worktree_code,
                path: path.clone(),
                head_mode: visible_index_entry.map(|entry| entry.mode),
                index_mode: visible_index_entry.map(|entry| entry.mode),
                worktree_mode: status_worktree_mode(
                    visible_index_entry,
                    worktree_entry,
                    worktree_present,
                ),
                head_oid: visible_index_entry.map(|entry| entry.oid),
                index_oid: visible_index_entry.map(|entry| entry.oid),
                submodule: submodule.filter(|sub| sub.any()),
            });
        }
    }
}

pub(crate) struct StatusComparisonInputs<'a> {
    head: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    index: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    worktree: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    tracked_presence: &'a HashSet<Vec<u8>>,
    stat_cache: &'a IndexStatCache,
    sparse_checkout_active: bool,
    submodule_dirt_map: &'a BTreeMap<Vec<u8>, u8>,
    ignores: &'a IgnoreMatcher,
}

pub(crate) fn collect_status_entries_with_head(
    inputs: StatusComparisonInputs<'_>,
    untracked_mode: StatusUntrackedMode,
    entries: &mut Vec<ShortStatusEntry>,
) {
    let mut paths = BTreeSet::new();
    paths.extend(inputs.head.keys().cloned());
    paths.extend(inputs.index.keys().cloned());
    paths.extend(
        inputs
            .worktree
            .keys()
            .filter(|path| inputs.index.contains_key(*path))
            .cloned(),
    );

    for path in paths {
        let head_entry = inputs.head.get(&path);
        let index_entry = inputs.index.get(&path);
        let intent_to_add = inputs
            .stat_cache
            .index_entry(&path)
            .is_some_and(IndexEntry::is_intent_to_add);
        let visible_index_entry = index_entry.filter(|_| !intent_to_add);
        let worktree_entry = inputs.worktree.get(&path);
        let worktree_present =
            worktree_entry.is_some() || inputs.tracked_presence.contains(path.as_slice());
        if head_entry.is_none()
            && index_entry.is_none()
            && worktree_entry.is_some()
            && inputs.ignores.is_ignored(&path, false)
        {
            continue;
        }
        let submodule = match visible_index_entry {
            Some(index_entry) => status_submodule_from_entries(
                &path,
                index_entry,
                worktree_entry,
                inputs.submodule_dirt_map,
                untracked_mode,
            ),
            None => None,
        };
        // Honor the skip-worktree bit literally unless sparse-checkout is active
        // and the file is present (git clears the bit then -> changes reported).
        let skip_worktree = visible_index_entry.is_some_and(|_| {
            inputs
                .stat_cache
                .index_entry(&path)
                .is_some_and(index_entry_skip_worktree)
        }) && (!inputs.sparse_checkout_active || !worktree_present);
        let (index_code, worktree_code) =
            if head_entry.is_none() && index_entry.is_none() && worktree_entry.is_some() {
                (b'?', b'?')
            } else {
                let index_code = match (head_entry, visible_index_entry) {
                    (None, Some(_)) => b'A',
                    (Some(_), None) => b'D',
                    (Some(left), Some(right)) if left != right => {
                        status_change_code(left.mode, right.mode)
                    }
                    _ => b' ',
                };
                let worktree_code = match (visible_index_entry, worktree_entry) {
                    (None, Some(_)) if intent_to_add => b'A',
                    (None, Some(_)) => b'?',
                    (None, None) if intent_to_add => b' ',
                    (Some(_), _) if skip_worktree => b' ',
                    (Some(_), None) if !worktree_present => b'D',
                    (Some(left), Some(right)) if left != right => {
                        status_change_code(left.mode, right.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                (index_code, worktree_code)
            };
        if index_code != b' ' || worktree_code != b' ' {
            let worktree_mode = if skip_worktree {
                visible_index_entry.map(|entry| entry.mode)
            } else {
                status_worktree_mode(visible_index_entry, worktree_entry, worktree_present)
            };
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path,
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: visible_index_entry.map(|entry| entry.mode),
                worktree_mode,
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: visible_index_entry.map(|entry| entry.oid),
                submodule: submodule.filter(|sub| sub.any()),
            });
        }
    }
}

pub(crate) fn status_worktree_mode(
    index_entry: Option<&TrackedEntry>,
    worktree_entry: Option<&TrackedEntry>,
    worktree_present: bool,
) -> Option<u32> {
    worktree_entry.map(|entry| entry.mode).or_else(|| {
        worktree_present
            .then(|| index_entry.map(|entry| entry.mode))
            .flatten()
    })
}

pub(crate) fn status_submodule_from_entries(
    path: &[u8],
    index_entry: &TrackedEntry,
    worktree_entry: Option<&TrackedEntry>,
    submodule_dirt_map: &BTreeMap<Vec<u8>, u8>,
    _untracked_mode: StatusUntrackedMode,
) -> Option<SubmoduleStatus> {
    let worktree_entry = worktree_entry?;
    if !sley_index::is_gitlink(index_entry.mode) || !sley_index::is_gitlink(worktree_entry.mode) {
        return None;
    }
    let dirt = submodule_dirt_map.get(path).copied().unwrap_or(0);
    Some(SubmoduleStatus {
        new_commits: index_entry.oid != worktree_entry.oid,
        modified_content: dirt & DIRTY_SUBMODULE_MODIFIED != 0,
        untracked_content: dirt & DIRTY_SUBMODULE_UNTRACKED != 0,
    })
}

pub(crate) fn short_status_tracked_only(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: &Index,
    stat_cache: &IndexStatCache,
    head_matches_index: bool,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let normal_entry_count = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    if head_matches_index && normal_entry_count >= 512 {
        return short_status_tracked_only_head_matches_index_parallel(
            worktree_root,
            git_dir,
            format,
            index,
            stat_cache,
            sparse_checkout_active,
            untracked_mode,
        );
    }
    let head = if head_matches_index {
        None
    } else {
        Some(head_tree_entries(git_dir, format, db)?)
    };
    if !head_matches_index && normal_entry_count >= 512 {
        if let Some(head) = head.as_ref() {
            return short_status_tracked_only_with_head_parallel(
                worktree_root,
                git_dir,
                format,
                index,
                stat_cache,
                head,
                sparse_checkout_active,
                untracked_mode,
            );
        }
    }
    let mut clean_filter = None;
    let mut entries = Vec::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
    {
        let path = entry.path.as_bytes();
        let index_entry = TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        };
        let head_entry = if head_matches_index {
            (!entry.is_intent_to_add()).then_some(&index_entry)
        } else {
            head.as_ref().and_then(|head| head.get(path))
        };
        let worktree_entry = worktree_entry_for_index_entry_with_attributes(
            worktree_root,
            git_dir,
            format,
            entry,
            stat_cache,
            &mut clean_filter,
        )?;
        let submodule = tracked_only_submodule_status(
            worktree_root,
            path,
            &index_entry,
            worktree_entry.as_ref(),
            untracked_mode,
        )?;
        let visible_index_entry = (!entry.is_intent_to_add()).then_some(&index_entry);
        let index_code = match (head_entry, visible_index_entry) {
            (None, Some(_)) => b'A',
            (Some(_), None) => b'D',
            (Some(head_entry), Some(index_entry)) if *head_entry != *index_entry => {
                status_change_code(head_entry.mode, index_entry.mode)
            }
            _ => b' ',
        };
        // Honor the skip-worktree bit literally unless sparse-checkout is active
        // and the file is present (git clears the bit then -> changes reported).
        let skip_worktree =
            entry.is_skip_worktree() && (!sparse_checkout_active || worktree_entry.is_none());
        let worktree_code = match worktree_entry.as_ref() {
            _ if skip_worktree => b' ',
            None if entry.is_intent_to_add() => b' ',
            None => b'D',
            Some(_) if entry.is_intent_to_add() => b'A',
            Some(worktree_entry) if Some(worktree_entry) != visible_index_entry => {
                status_change_code(index_entry.mode, worktree_entry.mode)
            }
            _ if submodule.is_some_and(|sub| sub.any()) => b'M',
            _ => b' ',
        };
        if index_code != b' ' || worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path: path.to_vec(),
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: visible_index_entry.map(|entry| entry.mode),
                worktree_mode: if skip_worktree {
                    visible_index_entry.map(|entry| entry.mode)
                } else {
                    worktree_entry.as_ref().map(|entry| entry.mode)
                },
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: visible_index_entry.map(|entry| entry.oid),
                submodule: submodule.filter(|sub| sub.any()),
            });
        }
    }
    if let Some(head) = head.as_ref() {
        let index_paths = index
            .entries
            .iter()
            .filter(|entry| entry.stage() == Stage::Normal)
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect::<HashSet<_>>();
        for (path, head_entry) in head {
            if index_paths.contains(path.as_slice()) {
                continue;
            }
            entries.push(ShortStatusEntry {
                index: b'D',
                worktree: b' ',
                path: path.clone(),
                head_mode: Some(head_entry.mode),
                index_mode: None,
                worktree_mode: None,
                head_oid: Some(head_entry.oid),
                index_oid: None,
                submodule: None,
            });
        }
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn short_status_borrowed_head_matches_index_if_possible(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    untracked_mode: StatusUntrackedMode,
) -> Result<Option<Vec<ShortStatusEntry>>> {
    let index_path = repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && matches!(untracked_mode, StatusUntrackedMode::None) =>
        {
            return Ok(Some(Vec::new()));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let index_bytes = read_borrowed_index_bytes(&index_path)?;
    status_profile_mem(
        "after_index_bytes",
        &[
            ("index_file_bytes", index_metadata.len() as usize),
            ("index_bytes_len", index_bytes.as_ref().len()),
            (
                "index_bytes_mapped",
                usize::from(matches!(index_bytes, BorrowedIndexBytes::Mapped(_))),
            ),
        ],
    );
    let borrowed = match BorrowedIndex::parse(index_bytes.as_ref(), format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    status_profile_mem(
        "after_borrowed_parse",
        &[
            ("index_file_bytes", index_metadata.len() as usize),
            ("index_bytes_len", index_bytes.as_ref().len()),
            (
                "index_bytes_mapped",
                usize::from(matches!(index_bytes, BorrowedIndexBytes::Mapped(_))),
            ),
            ("borrowed_entries_len", borrowed.entries.len()),
            ("borrowed_entries_cap", borrowed.entries.capacity()),
            (
                "borrowed_entry_size",
                std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            ("borrowed_extensions_len", borrowed.extensions.len()),
        ],
    );
    let sparse_checkout_active = sparse_checkout_active_for_borrowed_status(git_dir, &borrowed);
    if borrowed
        .entries
        .iter()
        .any(|entry| entry.mode == SPARSE_DIR_MODE && entry.is_skip_worktree())
    {
        return Ok(None);
    }
    if borrowed
        .entries
        .iter()
        .any(|entry| entry.stage() != Stage::Normal)
    {
        return Ok(None);
    }
    status_profile_mem(
        "after_sparse_scan",
        &[
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            (
                "sparse_checkout_active",
                usize::from(sparse_checkout_active),
            ),
        ],
    );
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    status_profile_mem(
        "after_head_tree_oid",
        &[
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
        ],
    );
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    status_profile_mem(
        "after_stage0_count",
        &[
            ("stage0_entry_count", stage0_entry_count),
            ("borrowed_entries_len", borrowed.entries.len()),
        ],
    );
    if !head_matches_borrowed_index_from_entries(&borrowed, format, &head_tree_oid)? {
        return Ok(None);
    }
    status_profile_mem(
        "after_head_matches_index",
        &[
            ("stage0_entry_count", stage0_entry_count),
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
        ],
    );

    let index_mtime = file_mtime_parts(&index_metadata);
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let profile_enabled = StatusProfileCounters::enabled();
    let mut profile = profile_enabled.then(|| StatusProfileCounters {
        fast_path_borrowed: true,
        ..StatusProfileCounters::default()
    });

    if matches!(untracked_mode, StatusUntrackedMode::None) {
        let tracked_start = Instant::now();
        let entries = short_status_borrowed_tracked_only_head_matches_index_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
            sparse_checkout_active,
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(entries));
    }

    if stage0_entry_count < STATUS_BORROWED_OVERLAP_MIN_STAGE0 {
        let tracked_start = Instant::now();
        let mut entries = short_status_borrowed_tracked_only_head_matches_index_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
            sparse_checkout_active,
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
        }
        let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
        let untracked_start = Instant::now();
        let untracked_paths = status_untracked_paths_from_borrowed_index(
            worktree_root,
            git_dir,
            &borrowed,
            &mut ignores,
            untracked_mode,
            profile.as_mut(),
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.untracked_elapsed_us = untracked_start.elapsed().as_micros();
            profile.untracked_rows = untracked_paths.len() as u64;
        }
        let render_start = Instant::now();
        append_untracked_status_entries(&mut entries, untracked_paths);
        if let Some(profile) = profile.as_mut() {
            profile.render_elapsed_us = render_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(entries));
    }

    if let Some(profile) = profile.as_mut() {
        profile.overlap_enabled = true;
    }
    let executor = StatusExecutor::new();
    if profile_enabled {
        let (mut entries, untracked_paths, untracked_profile) =
            std::thread::scope(|scope| -> Result<_> {
                let tracked = executor.spawn(scope, "status-tracked", || {
                    let start = Instant::now();
                    short_status_borrowed_tracked_only_head_matches_index_parallel(
                        worktree_root,
                        git_dir,
                        format,
                        &borrowed,
                        &stat_cache,
                        sparse_checkout_active,
                        untracked_mode,
                    )
                    .map(|entries| (entries, start.elapsed().as_micros()))
                })?;
                let untracked = executor.spawn(
                    scope,
                    "status-untracked",
                    || -> Result<(Vec<Vec<u8>>, StatusProfileCounters)> {
                        let mut local_profile = StatusProfileCounters::default();
                        let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
                        let start = Instant::now();
                        let paths = status_untracked_paths_from_borrowed_index(
                            worktree_root,
                            git_dir,
                            &borrowed,
                            &mut ignores,
                            untracked_mode,
                            Some(&mut local_profile),
                        )?;
                        local_profile.untracked_elapsed_us = start.elapsed().as_micros();
                        local_profile.untracked_rows = paths.len() as u64;
                        Ok((paths, local_profile))
                    },
                )?;
                let (entries, tracked_elapsed_us) = tracked.join()?;
                let (untracked_paths, untracked_profile) = untracked.join()?;
                if let Some(profile) = profile.as_mut() {
                    profile.tracked_elapsed_us = tracked_elapsed_us;
                }
                Ok((entries, untracked_paths, Some(untracked_profile)))
            })?;
        if let Some(profile) = profile.as_mut() {
            if let Some(untracked_profile) = untracked_profile {
                profile.merge_untracked(untracked_profile);
            }
        }
        let render_start = Instant::now();
        append_untracked_status_entries(&mut entries, untracked_paths);
        if let Some(profile) = profile.as_mut() {
            profile.render_elapsed_us = render_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(entries));
    }
    let (mut entries, untracked_paths) = std::thread::scope(|scope| -> Result<_> {
        let tracked = executor.spawn(scope, "status-tracked", || {
            short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
                sparse_checkout_active,
                untracked_mode,
            )
        })?;
        let untracked = executor.spawn(scope, "status-untracked", || -> Result<Vec<Vec<u8>>> {
            let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
            status_untracked_paths_from_borrowed_index(
                worktree_root,
                git_dir,
                &borrowed,
                &mut ignores,
                untracked_mode,
                None,
            )
        })?;
        let entries = tracked.join()?;
        let untracked_paths = untracked.join()?;
        Ok((entries, untracked_paths))
    })?;
    let render_start = Instant::now();
    append_untracked_status_entries(&mut entries, untracked_paths);
    if let Some(profile) = profile.as_mut() {
        profile.render_elapsed_us = render_start.elapsed().as_micros();
        profile.emit();
    }
    Ok(Some(entries))
}

pub(crate) fn stream_short_status_borrowed_head_matches_index_if_possible<F>(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    untracked_mode: StatusUntrackedMode,
    emit: &mut F,
) -> Result<Option<()>>
where
    F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
{
    let index_path = repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && matches!(untracked_mode, StatusUntrackedMode::None) =>
        {
            return Ok(Some(()));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let index_bytes = read_borrowed_index_bytes(&index_path)?;
    status_profile_mem(
        "after_index_bytes",
        &[
            ("index_file_bytes", index_metadata.len() as usize),
            ("index_bytes_len", index_bytes.as_ref().len()),
            (
                "index_bytes_mapped",
                usize::from(matches!(index_bytes, BorrowedIndexBytes::Mapped(_))),
            ),
        ],
    );
    let borrowed = match BorrowedIndex::parse(index_bytes.as_ref(), format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    status_profile_mem(
        "after_borrowed_parse",
        &[
            ("index_file_bytes", index_metadata.len() as usize),
            ("index_bytes_len", index_bytes.as_ref().len()),
            (
                "index_bytes_mapped",
                usize::from(matches!(index_bytes, BorrowedIndexBytes::Mapped(_))),
            ),
            ("borrowed_entries_len", borrowed.entries.len()),
            ("borrowed_entries_cap", borrowed.entries.capacity()),
            (
                "borrowed_entry_size",
                std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            ("borrowed_extensions_len", borrowed.extensions.len()),
        ],
    );
    let sparse_checkout_active = sparse_checkout_active_for_borrowed_status(git_dir, &borrowed);
    if borrowed
        .entries
        .iter()
        .any(|entry| entry.mode == SPARSE_DIR_MODE && entry.is_skip_worktree())
    {
        return Ok(None);
    }
    if borrowed
        .entries
        .iter()
        .any(|entry| entry.stage() != Stage::Normal)
    {
        return Ok(None);
    }
    status_profile_mem(
        "after_sparse_scan",
        &[
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
            (
                "sparse_checkout_active",
                usize::from(sparse_checkout_active),
            ),
        ],
    );
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    status_profile_mem(
        "after_head_tree_oid",
        &[
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
        ],
    );
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    status_profile_mem(
        "after_stage0_count",
        &[
            ("stage0_entry_count", stage0_entry_count),
            ("borrowed_entries_len", borrowed.entries.len()),
        ],
    );
    if !head_matches_borrowed_index_from_entries(&borrowed, format, &head_tree_oid)? {
        return Ok(None);
    }
    status_profile_mem(
        "after_head_matches_index",
        &[
            ("stage0_entry_count", stage0_entry_count),
            ("borrowed_entries_len", borrowed.entries.len()),
            (
                "borrowed_entries_cap_bytes",
                borrowed.entries.capacity() * std::mem::size_of::<IndexEntryRef<'_>>(),
            ),
        ],
    );

    let index_mtime = file_mtime_parts(&index_metadata);
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let profile_enabled = StatusProfileCounters::enabled();
    let mut profile = profile_enabled.then(|| StatusProfileCounters {
        fast_path_borrowed: true,
        ..StatusProfileCounters::default()
    });

    if matches!(untracked_mode, StatusUntrackedMode::None) {
        let tracked_start = Instant::now();
        let tracked_control =
            stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
                sparse_checkout_active,
                untracked_mode,
                emit,
            )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
        }
        if let Some(profile) = profile.as_ref() {
            profile.emit();
        }
        if tracked_control.is_stop() {
            return Ok(Some(()));
        }
        return Ok(Some(()));
    }

    if stage0_entry_count < STATUS_BORROWED_OVERLAP_MIN_STAGE0 {
        let tracked_start = Instant::now();
        let tracked_control =
            stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
                sparse_checkout_active,
                untracked_mode,
                emit,
            )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
        }
        if tracked_control.is_stop() {
            if let Some(profile) = profile.as_ref() {
                profile.emit();
            }
            return Ok(Some(()));
        }
        let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
        let untracked_start = Instant::now();
        stream_status_untracked_paths_from_borrowed_index(
            worktree_root,
            git_dir,
            &borrowed,
            &mut ignores,
            untracked_mode,
            profile.as_mut(),
            emit_untracked_status_entry(emit),
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.untracked_elapsed_us = untracked_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(()));
    }

    if let Some(profile) = profile.as_mut() {
        profile.overlap_enabled = true;
    }
    let executor = StatusExecutor::new();
    let (tracked_control, untracked_paths, untracked_profile) =
        std::thread::scope(|scope| -> Result<_> {
            let untracked = executor.spawn(
                scope,
                "status-untracked",
                || -> Result<(Vec<Vec<u8>>, StatusProfileCounters)> {
                    let mut local_profile = StatusProfileCounters::default();
                    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
                    ignores.emit_memory_profile("after_untracked_ignore");
                    let start = Instant::now();
                    let paths = status_untracked_paths_from_borrowed_index(
                        worktree_root,
                        git_dir,
                        &borrowed,
                        &mut ignores,
                        untracked_mode,
                        profile_enabled.then_some(&mut local_profile),
                    )?;
                    status_profile_mem(
                        "after_untracked_collect",
                        &[
                            ("untracked_paths_len", paths.len()),
                            ("untracked_paths_cap", paths.capacity()),
                            (
                                "untracked_paths_cap_bytes",
                                paths.capacity() * std::mem::size_of::<Vec<u8>>(),
                            ),
                            (
                                "untracked_path_payload_bytes",
                                paths.iter().map(Vec::capacity).sum(),
                            ),
                        ],
                    );
                    local_profile.untracked_elapsed_us = start.elapsed().as_micros();
                    local_profile.untracked_rows = paths.len() as u64;
                    Ok((paths, local_profile))
                },
            )?;
            let tracked_start = Instant::now();
            let tracked_control =
                stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                    worktree_root,
                    git_dir,
                    format,
                    &borrowed,
                    &stat_cache,
                    sparse_checkout_active,
                    untracked_mode,
                    emit,
                )?;
            let tracked_elapsed_us = tracked_start.elapsed().as_micros();
            let (untracked_paths, untracked_profile) = untracked.join()?;
            if let Some(profile) = profile.as_mut() {
                profile.tracked_elapsed_us = tracked_elapsed_us;
            }
            Ok((
                tracked_control,
                untracked_paths,
                profile_enabled.then_some(untracked_profile),
            ))
        })?;
    status_profile_mem(
        "after_join",
        &[
            ("untracked_paths_len", untracked_paths.len()),
            ("untracked_paths_cap", untracked_paths.capacity()),
            (
                "untracked_paths_cap_bytes",
                untracked_paths.capacity() * std::mem::size_of::<Vec<u8>>(),
            ),
            (
                "untracked_path_payload_bytes",
                untracked_paths.iter().map(Vec::capacity).sum(),
            ),
        ],
    );
    if tracked_control.is_stop() {
        if let Some(profile) = profile.as_mut()
            && let Some(untracked_profile) = untracked_profile
        {
            profile.merge_untracked(untracked_profile);
            profile.emit();
        }
        return Ok(Some(()));
    }
    if let Some(profile) = profile.as_mut()
        && let Some(untracked_profile) = untracked_profile
    {
        profile.merge_untracked(untracked_profile);
    }
    let render_start = Instant::now();
    for path in untracked_paths {
        let row = untracked_status_row(&path);
        if emit(row)?.is_stop() {
            break;
        }
    }
    if let Some(profile) = profile.as_mut() {
        profile.render_elapsed_us = render_start.elapsed().as_micros();
        profile.emit();
    }
    status_profile_mem("after_render", &[]);
    Ok(Some(()))
}

pub(crate) fn short_status_borrowed_head_matches_index_count_if_possible(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    untracked_mode: StatusUntrackedMode,
) -> Result<Option<usize>> {
    let index_path = repository_index_path(git_dir);
    let index_metadata = match fs::metadata(&index_path) {
        Ok(metadata) => metadata,
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && matches!(untracked_mode, StatusUntrackedMode::None) =>
        {
            return Ok(Some(0));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let index_bytes = read_borrowed_index_bytes(&index_path)?;
    let borrowed = match BorrowedIndex::parse(index_bytes.as_ref(), format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let sparse_checkout_active = sparse_checkout_active_for_borrowed_status(git_dir, &borrowed);
    if borrowed
        .entries
        .iter()
        .any(|entry| entry.mode == SPARSE_DIR_MODE && entry.is_skip_worktree())
    {
        return Ok(None);
    }
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    if !head_matches_borrowed_index_from_entries(&borrowed, format, &head_tree_oid)? {
        return Ok(None);
    }

    let index_mtime = file_mtime_parts(&index_metadata);
    let stat_cache = IndexStatCache::from_index_mtime_only(index_mtime);
    let profile_enabled = StatusProfileCounters::enabled();
    let mut profile = profile_enabled.then(|| StatusProfileCounters {
        fast_path_borrowed: true,
        ..StatusProfileCounters::default()
    });

    if matches!(untracked_mode, StatusUntrackedMode::None) {
        let tracked_start = Instant::now();
        let count = short_status_borrowed_tracked_only_head_matches_index_count_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
            sparse_checkout_active,
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(count));
    }

    if stage0_entry_count < STATUS_BORROWED_OVERLAP_MIN_STAGE0 {
        let tracked_start = Instant::now();
        let tracked_count = short_status_borrowed_tracked_only_head_matches_index_count_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
            sparse_checkout_active,
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
        }
        let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
        let untracked_start = Instant::now();
        let untracked_count = status_untracked_count_from_borrowed_index(
            worktree_root,
            git_dir,
            &borrowed,
            &mut ignores,
            untracked_mode,
            profile.as_mut(),
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.untracked_elapsed_us = untracked_start.elapsed().as_micros();
            profile.untracked_rows = untracked_count as u64;
            profile.emit();
        }
        return Ok(Some(tracked_count + untracked_count));
    }

    if let Some(profile) = profile.as_mut() {
        profile.overlap_enabled = true;
    }
    let executor = StatusExecutor::new();
    let (tracked_count, untracked_count, untracked_profile) =
        std::thread::scope(|scope| -> Result<_> {
            let tracked = executor.spawn(scope, "status-tracked", || {
                let start = Instant::now();
                short_status_borrowed_tracked_only_head_matches_index_count_parallel(
                    worktree_root,
                    git_dir,
                    format,
                    &borrowed,
                    &stat_cache,
                    sparse_checkout_active,
                    untracked_mode,
                )
                .map(|count| (count, start.elapsed().as_micros()))
            })?;
            let untracked = executor.spawn(
                scope,
                "status-untracked",
                || -> Result<(usize, StatusProfileCounters)> {
                    let mut local_profile = StatusProfileCounters::default();
                    let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
                    let start = Instant::now();
                    let count = status_untracked_count_from_borrowed_index(
                        worktree_root,
                        git_dir,
                        &borrowed,
                        &mut ignores,
                        untracked_mode,
                        profile_enabled.then_some(&mut local_profile),
                    )?;
                    local_profile.untracked_elapsed_us = start.elapsed().as_micros();
                    local_profile.untracked_rows = count as u64;
                    Ok((count, local_profile))
                },
            )?;
            let (tracked_count, tracked_elapsed_us) = tracked.join()?;
            let (untracked_count, untracked_profile) = untracked.join()?;
            if let Some(profile) = profile.as_mut() {
                profile.tracked_elapsed_us = tracked_elapsed_us;
            }
            Ok((
                tracked_count,
                untracked_count,
                profile_enabled.then_some(untracked_profile),
            ))
        })?;
    if let Some(profile) = profile.as_mut() {
        if let Some(untracked_profile) = untracked_profile {
            profile.merge_untracked(untracked_profile);
        }
        profile.emit();
    }
    Ok(Some(tracked_count + untracked_count))
}

pub(crate) fn emit_untracked_status_entry<'a, F>(
    emit: &'a mut F,
) -> impl FnMut(&[u8]) -> Result<StreamControl> + 'a
where
    F: for<'row> FnMut(ShortStatusRow<'row>) -> Result<StreamControl>,
{
    |path| emit(untracked_status_row(path))
}

pub(crate) fn untracked_status_entry(path: Vec<u8>) -> ShortStatusEntry {
    ShortStatusEntry {
        index: b'?',
        worktree: b'?',
        path,
        head_mode: None,
        index_mode: None,
        worktree_mode: None,
        head_oid: None,
        index_oid: None,
        submodule: None,
    }
}

pub(crate) fn untracked_status_row(path: &[u8]) -> ShortStatusRow<'_> {
    ShortStatusRow {
        index: b'?',
        worktree: b'?',
        path,
        head_mode: None,
        index_mode: None,
        worktree_mode: None,
        head_oid: None,
        index_oid: None,
        submodule: None,
    }
}

pub(crate) fn append_untracked_status_entries(
    entries: &mut Vec<ShortStatusEntry>,
    untracked_paths: Vec<Vec<u8>>,
) {
    for path in untracked_paths {
        entries.push(untracked_status_entry(path));
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TrackedOnlyPrecheck {
    Deleted(usize),
    Slow(usize),
}

#[derive(Debug)]
pub(crate) enum TrackedOnlyPrecheckOutcome {
    Clean,
    Deleted,
    Slow,
}

pub(crate) fn short_status_tracked_only_head_matches_index_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks = tracked_only_non_clean_prechecks_parallel(
        worktree_root,
        index,
        stat_cache,
        sparse_checkout_active,
    )?;

    let mut clean_filter = None;
    let mut entries = Vec::new();
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
                if entry.is_intent_to_add() {
                    continue;
                }
                let path = entry.path.as_bytes();
                entries.push(ShortStatusEntry {
                    index: b' ',
                    worktree: b'D',
                    path: path.to_vec(),
                    head_mode: Some(entry.mode),
                    index_mode: Some(entry.mode),
                    worktree_mode: None,
                    head_oid: Some(entry.oid),
                    index_oid: Some(entry.oid),
                    submodule: None,
                });
            }
            TrackedOnlyPrecheck::Slow(idx) => {
                let entry = &index.entries[idx];
                let path = entry.path.as_bytes();
                let index_entry = TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                };
                let worktree_entry = worktree_entry_for_index_entry_with_attributes(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    stat_cache,
                    &mut clean_filter,
                )?;
                let submodule = tracked_only_submodule_status(
                    worktree_root,
                    path,
                    &index_entry,
                    worktree_entry.as_ref(),
                    untracked_mode,
                )?;
                let worktree_code = match worktree_entry.as_ref() {
                    None if entry.is_intent_to_add() => b' ',
                    None => b'D',
                    Some(_) if entry.is_intent_to_add() => b'A',
                    Some(worktree_entry) if *worktree_entry != index_entry => {
                        status_change_code(index_entry.mode, worktree_entry.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    entries.push(ShortStatusEntry {
                        index: b' ',
                        worktree: worktree_code,
                        path: path.to_vec(),
                        head_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        index_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        index_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        submodule: submodule.filter(|sub| sub.any()),
                    });
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn short_status_borrowed_tracked_only_head_matches_index_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks = tracked_only_borrowed_non_clean_prechecks_parallel(
        worktree_root,
        index,
        stat_cache,
        sparse_checkout_active,
    )?;

    let mut clean_filter = None;
    let mut entries = Vec::new();
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
                if entry.is_intent_to_add() {
                    continue;
                }
                entries.push(ShortStatusEntry {
                    index: b' ',
                    worktree: b'D',
                    path: entry.path.to_vec(),
                    head_mode: Some(entry.mode),
                    index_mode: Some(entry.mode),
                    worktree_mode: None,
                    head_oid: Some(entry.oid),
                    index_oid: Some(entry.oid),
                    submodule: None,
                });
            }
            TrackedOnlyPrecheck::Slow(idx) => {
                let entry = &index.entries[idx];
                let index_entry = TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                };
                let worktree_entry = worktree_entry_for_index_entry_ref_with_attributes(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    stat_cache,
                    &mut clean_filter,
                )?;
                let submodule = tracked_only_submodule_status(
                    worktree_root,
                    entry.path,
                    &index_entry,
                    worktree_entry.as_ref(),
                    untracked_mode,
                )?;
                let worktree_code = match worktree_entry.as_ref() {
                    None if entry.is_intent_to_add() => b' ',
                    None => b'D',
                    Some(_) if entry.is_intent_to_add() => b'A',
                    Some(worktree_entry) if *worktree_entry != index_entry => {
                        status_change_code(index_entry.mode, worktree_entry.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    entries.push(ShortStatusEntry {
                        index: b' ',
                        worktree: worktree_code,
                        path: entry.path.to_vec(),
                        head_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        index_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        index_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        submodule: submodule.filter(|sub| sub.any()),
                    });
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn stream_short_status_borrowed_tracked_only_head_matches_index_parallel<F>(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
    emit: &mut F,
) -> Result<StreamControl>
where
    F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
{
    let prechecks = tracked_only_borrowed_non_clean_prechecks_parallel(
        worktree_root,
        index,
        stat_cache,
        sparse_checkout_active,
    )?;

    let mut clean_filter = None;
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
                if entry.is_intent_to_add() {
                    continue;
                }
                if emit(ShortStatusRow {
                    index: b' ',
                    worktree: b'D',
                    path: entry.path,
                    head_mode: Some(entry.mode),
                    index_mode: Some(entry.mode),
                    worktree_mode: None,
                    head_oid: Some(entry.oid),
                    index_oid: Some(entry.oid),
                    submodule: None,
                })?
                .is_stop()
                {
                    return Ok(StreamControl::Stop);
                }
            }
            TrackedOnlyPrecheck::Slow(idx) => {
                let entry = &index.entries[idx];
                let index_entry = TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                };
                let worktree_entry = worktree_entry_for_index_entry_ref_with_attributes(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    stat_cache,
                    &mut clean_filter,
                )?;
                let submodule = tracked_only_submodule_status(
                    worktree_root,
                    entry.path,
                    &index_entry,
                    worktree_entry.as_ref(),
                    untracked_mode,
                )?;
                let worktree_code = match worktree_entry.as_ref() {
                    None if entry.is_intent_to_add() => b' ',
                    None => b'D',
                    Some(_) if entry.is_intent_to_add() => b'A',
                    Some(worktree_entry) if *worktree_entry != index_entry => {
                        status_change_code(index_entry.mode, worktree_entry.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    if emit(ShortStatusRow {
                        index: b' ',
                        worktree: worktree_code,
                        path: entry.path,
                        head_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        index_mode: (!entry.is_intent_to_add()).then_some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        index_oid: (!entry.is_intent_to_add()).then_some(index_entry.oid),
                        submodule: submodule.filter(|sub| sub.any()),
                    })?
                    .is_stop()
                    {
                        return Ok(StreamControl::Stop);
                    }
                }
            }
        }
    }
    Ok(StreamControl::Continue)
}

pub(crate) fn short_status_borrowed_tracked_only_head_matches_index_count_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
) -> Result<usize> {
    let prechecks = tracked_only_borrowed_non_clean_prechecks_parallel(
        worktree_root,
        index,
        stat_cache,
        sparse_checkout_active,
    )?;

    let mut clean_filter = None;
    let mut count = 0usize;
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(_) => count += 1,
            TrackedOnlyPrecheck::Slow(idx) => {
                let entry = &index.entries[idx];
                let index_entry = TrackedEntry {
                    mode: entry.mode,
                    oid: entry.oid,
                };
                let worktree_entry = worktree_entry_for_index_entry_ref_with_attributes(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    stat_cache,
                    &mut clean_filter,
                )?;
                let submodule = tracked_only_submodule_status(
                    worktree_root,
                    entry.path,
                    &index_entry,
                    worktree_entry.as_ref(),
                    untracked_mode,
                )?;
                let worktree_code = match worktree_entry.as_ref() {
                    None => b'D',
                    Some(worktree_entry) if *worktree_entry != index_entry => {
                        status_change_code(index_entry.mode, worktree_entry.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    count += 1;
                }
            }
        }
    }
    Ok(count)
}

pub(crate) fn short_status_tracked_only_with_head_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    stat_cache: &IndexStatCache,
    head: &BTreeMap<Vec<u8>, TrackedEntry>,
    sparse_checkout_active: bool,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks = tracked_only_non_clean_prechecks_parallel(
        worktree_root,
        index,
        stat_cache,
        sparse_checkout_active,
    )?;
    let mut precheck_cursor = 0usize;
    let mut clean_filter = None;
    let mut entries = Vec::new();

    for (idx, entry) in index.entries.iter().enumerate() {
        if entry.stage() != Stage::Normal {
            continue;
        }
        let path = entry.path.as_bytes();
        let index_entry = TrackedEntry {
            mode: entry.mode,
            oid: entry.oid,
        };
        let head_entry = head.get(path);
        let visible_index_entry = (!entry.is_intent_to_add()).then_some(&index_entry);
        let index_code = match (head_entry, visible_index_entry) {
            (None, Some(_)) => b'A',
            (Some(_), None) => b'D',
            (Some(head_entry), Some(index_entry)) if *head_entry != *index_entry => {
                status_change_code(head_entry.mode, index_entry.mode)
            }
            _ => b' ',
        };
        let precheck = prechecks
            .get(precheck_cursor)
            .copied()
            .and_then(|precheck| {
                if tracked_only_precheck_index(precheck) == idx {
                    precheck_cursor += 1;
                    Some(precheck)
                } else {
                    None
                }
            });
        let (worktree_code, worktree_mode, submodule) = match precheck {
            None if entry.is_intent_to_add() => (b' ', None, None),
            None => (b' ', Some(index_entry.mode), None),
            Some(TrackedOnlyPrecheck::Deleted(_)) if entry.is_intent_to_add() => (b' ', None, None),
            Some(TrackedOnlyPrecheck::Deleted(_)) => (b'D', None, None),
            Some(TrackedOnlyPrecheck::Slow(_)) => {
                let worktree_entry = worktree_entry_for_index_entry_with_attributes(
                    worktree_root,
                    git_dir,
                    format,
                    entry,
                    stat_cache,
                    &mut clean_filter,
                )?;
                let submodule = tracked_only_submodule_status(
                    worktree_root,
                    path,
                    &index_entry,
                    worktree_entry.as_ref(),
                    untracked_mode,
                )?;
                let worktree_code = match worktree_entry.as_ref() {
                    None if entry.is_intent_to_add() => b' ',
                    None => b'D',
                    Some(_) if entry.is_intent_to_add() => b'A',
                    Some(worktree_entry) if *worktree_entry != index_entry => {
                        status_change_code(index_entry.mode, worktree_entry.mode)
                    }
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                (
                    worktree_code,
                    worktree_entry.as_ref().map(|entry| entry.mode),
                    submodule.filter(|sub| sub.any()),
                )
            }
        };
        if index_code != b' ' || worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path: path.to_vec(),
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: visible_index_entry.map(|entry| entry.mode),
                worktree_mode,
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: visible_index_entry.map(|entry| entry.oid),
                submodule,
            });
        }
    }

    let index_paths = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect::<HashSet<_>>();
    for (path, head_entry) in head {
        if index_paths.contains(path.as_slice()) {
            continue;
        }
        entries.push(ShortStatusEntry {
            index: b'D',
            worktree: b' ',
            path: path.clone(),
            head_mode: Some(head_entry.mode),
            index_mode: None,
            worktree_mode: None,
            head_oid: Some(head_entry.oid),
            index_oid: None,
            submodule: None,
        });
    }
    entries.sort_by(|left, right| {
        status_sort_category(left)
            .cmp(&status_sort_category(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn tracked_only_precheck_index(precheck: TrackedOnlyPrecheck) -> usize {
    match precheck {
        TrackedOnlyPrecheck::Deleted(idx) | TrackedOnlyPrecheck::Slow(idx) => idx,
    }
}

pub(crate) fn stage0_index_entry_count<E>(
    entries: &[E],
    mut stage: impl FnMut(&E) -> Stage,
) -> usize {
    entries
        .iter()
        .filter(|entry| stage(entry) == Stage::Normal)
        .count()
}

pub(crate) fn stage0_index_chunk_ranges<E>(
    entries: &[E],
    chunk_size: usize,
    mut stage: impl FnMut(&E) -> Stage,
) -> Vec<std::ops::Range<usize>> {
    debug_assert!(chunk_size > 0);
    let mut ranges = Vec::new();
    let mut start = None;
    let mut end = 0usize;
    let mut normals_in_chunk = 0usize;
    for (idx, entry) in entries.iter().enumerate() {
        if stage(entry) != Stage::Normal {
            continue;
        }
        if start.is_none() {
            start = Some(idx);
        }
        end = idx + 1;
        normals_in_chunk += 1;
        if normals_in_chunk == chunk_size {
            if let Some(start) = start {
                ranges.push(start..end);
            }
            start = None;
            normals_in_chunk = 0;
        }
    }
    if let Some(start) = start {
        ranges.push(start..end);
    }
    ranges
}

pub(crate) fn tracked_only_non_clean_prechecks_parallel(
    worktree_root: &Path,
    index: &Index,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
) -> Result<Vec<TrackedOnlyPrecheck>> {
    let normal_count = stage0_index_entry_count(&index.entries, IndexEntry::stage);
    if normal_count == 0 {
        return Ok(Vec::new());
    }
    let executor = StatusExecutor::new();
    let worker_count = executor.worker_count_for(normal_count, 512, 4);
    if worker_count == 1 {
        let mut prechecks = Vec::new();
        let mut absolute = PathBuf::new();
        for (idx, entry) in index.entries.iter().enumerate() {
            if entry.stage() != Stage::Normal {
                continue;
            }
            match tracked_only_stat_precheck(
                worktree_root,
                entry,
                stat_cache,
                sparse_checkout_active,
                &mut absolute,
            )? {
                TrackedOnlyPrecheckOutcome::Clean => {}
                TrackedOnlyPrecheckOutcome::Deleted => {
                    prechecks.push(TrackedOnlyPrecheck::Deleted(idx));
                }
                TrackedOnlyPrecheckOutcome::Slow => {
                    prechecks.push(TrackedOnlyPrecheck::Slow(idx));
                }
            }
        }
        return Ok(prechecks);
    }
    let chunk_size = normal_count.div_ceil(worker_count);
    let chunk_ranges = stage0_index_chunk_ranges(&index.entries, chunk_size, IndexEntry::stage);
    let next_chunk = AtomicUsize::new(0);
    let mut prechecks = std::thread::scope(|scope| -> Result<Vec<TrackedOnlyPrecheck>> {
        let mut handles = Vec::new();
        for _ in 0..worker_count {
            let chunk_ranges = &chunk_ranges;
            let next_chunk = &next_chunk;
            handles.push(executor.spawn(
                scope,
                "status-precheck",
                move || -> Result<Vec<TrackedOnlyPrecheck>> {
                    let mut prechecks = Vec::new();
                    let mut absolute = PathBuf::new();
                    loop {
                        let chunk_idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                        let Some(range) = chunk_ranges.get(chunk_idx) else {
                            break;
                        };
                        for idx in range.clone() {
                            let entry = &index.entries[idx];
                            if entry.stage() != Stage::Normal {
                                continue;
                            }
                            match tracked_only_stat_precheck(
                                worktree_root,
                                entry,
                                stat_cache,
                                sparse_checkout_active,
                                &mut absolute,
                            )? {
                                TrackedOnlyPrecheckOutcome::Clean => {}
                                TrackedOnlyPrecheckOutcome::Deleted => {
                                    prechecks.push(TrackedOnlyPrecheck::Deleted(idx));
                                }
                                TrackedOnlyPrecheckOutcome::Slow => {
                                    prechecks.push(TrackedOnlyPrecheck::Slow(idx));
                                }
                            }
                        }
                    }
                    Ok(prechecks)
                },
            )?);
        }
        let mut prechecks = Vec::new();
        for handle in handles {
            let mut chunk = handle.join()?;
            prechecks.append(&mut chunk);
        }
        Ok(prechecks)
    })?;
    prechecks.sort_by_key(|precheck| tracked_only_precheck_index(*precheck));
    Ok(prechecks)
}

pub(crate) fn tracked_only_borrowed_non_clean_prechecks_parallel(
    worktree_root: &Path,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
) -> Result<Vec<TrackedOnlyPrecheck>> {
    let normal_count = stage0_index_entry_count(&index.entries, IndexEntryRef::stage);
    if normal_count == 0 {
        return Ok(Vec::new());
    }
    let executor = StatusExecutor::new();
    let worker_count = executor.worker_count_for(normal_count, 512, 4);
    if worker_count == 1 {
        let mut prechecks = Vec::new();
        let mut absolute = PathBuf::new();
        for (idx, entry) in index.entries.iter().enumerate() {
            if entry.stage() != Stage::Normal {
                continue;
            }
            match tracked_only_borrowed_stat_precheck(
                worktree_root,
                entry,
                stat_cache,
                sparse_checkout_active,
                &mut absolute,
            )? {
                TrackedOnlyPrecheckOutcome::Clean => {}
                TrackedOnlyPrecheckOutcome::Deleted => {
                    prechecks.push(TrackedOnlyPrecheck::Deleted(idx));
                }
                TrackedOnlyPrecheckOutcome::Slow => {
                    prechecks.push(TrackedOnlyPrecheck::Slow(idx));
                }
            }
        }
        return Ok(prechecks);
    }
    let chunk_size = normal_count.div_ceil(worker_count);
    let chunk_ranges = stage0_index_chunk_ranges(&index.entries, chunk_size, IndexEntryRef::stage);
    let next_chunk = AtomicUsize::new(0);
    let mut prechecks = std::thread::scope(|scope| -> Result<Vec<TrackedOnlyPrecheck>> {
        let mut handles = Vec::new();
        for _ in 0..worker_count {
            let chunk_ranges = &chunk_ranges;
            let next_chunk = &next_chunk;
            handles.push(executor.spawn(
                scope,
                "status-precheck",
                move || -> Result<Vec<TrackedOnlyPrecheck>> {
                    let mut prechecks = Vec::new();
                    let mut absolute = PathBuf::new();
                    loop {
                        let chunk_idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                        let Some(range) = chunk_ranges.get(chunk_idx) else {
                            break;
                        };
                        for idx in range.clone() {
                            let entry = &index.entries[idx];
                            if entry.stage() != Stage::Normal {
                                continue;
                            }
                            match tracked_only_borrowed_stat_precheck(
                                worktree_root,
                                entry,
                                stat_cache,
                                sparse_checkout_active,
                                &mut absolute,
                            )? {
                                TrackedOnlyPrecheckOutcome::Clean => {}
                                TrackedOnlyPrecheckOutcome::Deleted => {
                                    prechecks.push(TrackedOnlyPrecheck::Deleted(idx));
                                }
                                TrackedOnlyPrecheckOutcome::Slow => {
                                    prechecks.push(TrackedOnlyPrecheck::Slow(idx));
                                }
                            }
                        }
                    }
                    Ok(prechecks)
                },
            )?);
        }
        let mut prechecks = Vec::new();
        for handle in handles {
            let mut chunk = handle.join()?;
            prechecks.append(&mut chunk);
        }
        Ok(prechecks)
    })?;
    prechecks.sort_by_key(|precheck| tracked_only_precheck_index(*precheck));
    Ok(prechecks)
}

pub(crate) fn tracked_only_stat_precheck(
    worktree_root: &Path,
    index_entry: &IndexEntry,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    absolute: &mut PathBuf,
) -> Result<TrackedOnlyPrecheckOutcome> {
    // Skip-worktree handling mirrors git's clear_skip_worktree_from_present_files:
    // when core.sparseCheckout is DISABLED the bit is honored literally, so the
    // worktree side is always clean regardless of the on-disk file (git's
    // run_diff_files skips ce_skip_worktree entries). When it is ENABLED, git
    // clears the bit for entries whose file is present, so a present file's
    // worktree changes ARE reported; only an absent file stays clean.
    if index_entry.is_skip_worktree() && !sparse_checkout_active {
        return Ok(TrackedOnlyPrecheckOutcome::Clean);
    }
    if sley_index::is_gitlink(index_entry.mode) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
    }
    let git_path = index_entry.path.as_bytes();
    if crate::index_io::git_path_has_symlink_parent(worktree_root, git_path)? {
        return Ok(TrackedOnlyPrecheckOutcome::Deleted);
    }
    set_worktree_path_from_repo_path(worktree_root, git_path, absolute)?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            // sparse-active + absent skip-worktree: bit stays set -> clean.
            if index_entry.is_skip_worktree() {
                return Ok(TrackedOnlyPrecheckOutcome::Clean);
            }
            return Ok(TrackedOnlyPrecheckOutcome::Deleted);
        }
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
    }
    if stat_cache
        .reuse_index_entry(index_entry, &metadata)
        .is_some()
    {
        Ok(TrackedOnlyPrecheckOutcome::Clean)
    } else {
        Ok(TrackedOnlyPrecheckOutcome::Slow)
    }
}

pub(crate) fn tracked_only_borrowed_stat_precheck(
    worktree_root: &Path,
    index_entry: &IndexEntryRef<'_>,
    stat_cache: &IndexStatCache,
    sparse_checkout_active: bool,
    absolute: &mut PathBuf,
) -> Result<TrackedOnlyPrecheckOutcome> {
    // See tracked_only_stat_precheck: when core.sparseCheckout is disabled the
    // skip-worktree bit is honored literally (worktree side always clean); when
    // enabled, git clears the bit for present files so their changes ARE
    // reported, and only absent files stay clean.
    if index_entry.is_skip_worktree() && !sparse_checkout_active {
        return Ok(TrackedOnlyPrecheckOutcome::Clean);
    }
    if sley_index::is_gitlink(index_entry.mode) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
    }
    if crate::index_io::git_path_has_symlink_parent(worktree_root, index_entry.path)? {
        return Ok(TrackedOnlyPrecheckOutcome::Deleted);
    }
    set_worktree_path_from_repo_path(worktree_root, index_entry.path, absolute)?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            // sparse-active + absent skip-worktree: bit stays set -> clean.
            if index_entry.is_skip_worktree() {
                return Ok(TrackedOnlyPrecheckOutcome::Clean);
            }
            return Ok(TrackedOnlyPrecheckOutcome::Deleted);
        }
        Err(err) => return Err(err.into()),
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() || !(file_type.is_file() || file_type.is_symlink()) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
    }
    if stat_cache
        .reuse_index_entry_ref(index_entry, &metadata)
        .is_some()
    {
        Ok(TrackedOnlyPrecheckOutcome::Clean)
    } else {
        Ok(TrackedOnlyPrecheckOutcome::Slow)
    }
}

pub(crate) fn set_worktree_path_from_repo_path(
    worktree_root: &Path,
    git_path: &[u8],
    out: &mut PathBuf,
) -> Result<()> {
    out.clear();
    out.push(worktree_root);
    push_repo_path(out, git_path)
}

#[cfg(unix)]
pub(crate) fn push_repo_path(out: &mut PathBuf, path: &[u8]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    out.push(Path::new(std::ffi::OsStr::from_bytes(path)));
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn push_repo_path(out: &mut PathBuf, path: &[u8]) -> Result<()> {
    let path = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidPath("index path is not utf8".into()))?;
    for component in path.split('/') {
        out.push(component);
    }
    Ok(())
}

pub(crate) fn tracked_only_submodule_status(
    worktree_root: &Path,
    path: &[u8],
    index_entry: &TrackedEntry,
    worktree_entry: Option<&TrackedEntry>,
    _untracked_mode: StatusUntrackedMode,
) -> Result<Option<SubmoduleStatus>> {
    let Some(worktree_entry) = worktree_entry else {
        return Ok(None);
    };
    if !sley_index::is_gitlink(index_entry.mode) || !sley_index::is_gitlink(worktree_entry.mode) {
        return Ok(None);
    }
    let absolute = worktree_root.join(repo_path_to_os_path(path)?);
    let dirt = if absolute.is_dir() {
        submodule_dirt_checked(&absolute)?
    } else {
        0
    };
    Ok(Some(SubmoduleStatus {
        new_commits: index_entry.oid != worktree_entry.oid,
        modified_content: dirt & DIRTY_SUBMODULE_MODIFIED != 0,
        untracked_content: dirt & DIRTY_SUBMODULE_UNTRACKED != 0,
    }))
}

pub(crate) fn status_sort_category(entry: &ShortStatusEntry) -> u8 {
    match (entry.index, entry.worktree) {
        (b'?', b'?') => 1,
        (b'!', b'!') => 2,
        _ => 0,
    }
}
