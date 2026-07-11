//! Public worktree/index/checkout result & option types, repository-index path helpers, and linked-worktree (shared-symref) administration.
//!
//! Split out of `lib.rs` in the wave-47 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;
use crate::checkout::*;
use crate::index::*;
use crate::index_io::*;
use crate::status::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeStatus {
    Clean,
    Modified(RepoPath),
    Added(RepoPath),
    Deleted(RepoPath),
    Untracked(RepoPath),
}

pub trait WorktreeScanner {
    fn status(&self) -> Result<Vec<WorktreeStatus>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseCheckout {
    pub patterns: Vec<Vec<u8>>,
    pub sparse_index: bool,
}

/// Selects how the patterns in a [`SparseCheckout`] are interpreted when
/// deciding which index paths are "in cone" (kept in the worktree).
///
/// * [`SparseCheckoutMode::Full`] interprets the patterns exactly like
///   `.gitignore` lines (full pattern matching, including `*`, `?`, `**`,
///   character classes, anchoring with a leading `/`, directory-only `/`
///   suffixes, and `!` negation). A path is *included* when the last pattern
///   that matches it is not negated. This mirrors upstream Git's non-cone
///   `core.sparseCheckout` behaviour.
/// * [`SparseCheckoutMode::Cone`] interprets the patterns as the restricted
///   directory-prefix form Git emits for `core.sparseCheckoutCone`: a literal
///   `/*` (top-level files), the recursive-parent guard `!/*/`, and anchored
///   directory patterns such as `/dir/` (everything under `dir/`) plus the
///   parent guards `/dir/*` and `!/dir/*/`. Matching is purely prefix based,
///   so glob metacharacters are treated literally.
/// * [`SparseCheckoutMode::Auto`] inspects the patterns and uses cone matching
///   when every pattern fits the cone grammar above, otherwise full matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SparseCheckoutMode {
    #[default]
    Auto,
    Full,
    Cone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplySparseResult {
    /// Paths whose worktree file was (re)materialized because they are in cone.
    pub materialized: Vec<Vec<u8>>,
    /// Paths that were taken out of the worktree because they are out of cone;
    /// their index entry now has the skip-worktree bit set.
    pub skipped: Vec<Vec<u8>>,
    /// Out-of-cone paths whose worktree file was *not* up to date with the index
    /// and was therefore left in place (and its skip-worktree bit left clear),
    /// matching git's data-loss-avoiding behavior. The caller surfaces these as
    /// git's "The following paths are not up to date …" warning. Sorted by path.
    pub not_up_to_date: Vec<Vec<u8>>,
    /// Paths with non-zero-stage index entries. Sparse application deliberately
    /// leaves these entries and their worktree files alone; the CLI renders
    /// Git's corresponding "paths are unmerged" warning. Sorted and unique.
    pub unmerged: Vec<Vec<u8>>,
    /// Out-of-cone tracked directory prefixes that could not be removed because
    /// they still contained non-ignored untracked files. Paths include a trailing
    /// slash, matching Git's sparse-directory spelling.
    pub untracked_sparse_directories: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateIndexResult {
    pub entries: usize,
    pub updated: Vec<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddUpdateTrackedAction {
    Add(Vec<u8>),
    Remove(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddExactTrackedPathResult {
    Handled(Option<AddUpdateTrackedAction>),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInfoEntry {
    pub mode: u32,
    pub oid: ObjectId,
    pub path: Vec<u8>,
    pub stage: u16,
}

/// Options for applying `update-index --cacheinfo` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateIndexCacheInfoOptions {
    pub add: bool,
    pub replace: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexInfoRecord {
    Add(CacheInfoEntry),
    Remove { path: Vec<u8> },
}

/// Batch-wide options for the `git add`-style callers that apply one uniform
/// mode to every path. The positional `add`/`remove`/`force_remove`/`info_only`/
/// `chmod` fields describe that uniform mode; `ignore_skip_worktree_entries` is
/// a genuine whole-invocation toggle (it is not positional in git either).
///
/// `git update-index <flag> <path>...` does NOT use this for its per-path mode —
/// it builds [`UpdateIndexPath`] values directly, each carrying the sticky mode
/// in effect when that path was parsed. See [`UpdateIndexPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateIndexOptions {
    pub add: bool,
    pub remove: bool,
    pub force_remove: bool,
    pub chmod: Option<bool>,
    pub info_only: bool,
    pub ignore_skip_worktree_entries: bool,
    pub allow_skip_worktree_entries: bool,
}

impl UpdateIndexOptions {
    /// The uniform per-path mode this batch applies to every path.
    pub(crate) fn path_mode(&self) -> UpdateIndexPathMode {
        UpdateIndexPathMode {
            add: self.add,
            remove: self.remove,
            force_remove: self.force_remove,
            info_only: self.info_only,
            chmod: self.chmod,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LargeObjectPolicy {
    pub(crate) threshold: u64,
    pub(crate) compression_level: u32,
    pub(crate) pack_size_limit: Option<u64>,
}

impl LargeObjectPolicy {
    pub(crate) fn from_config(git_dir: &Path, parameters_env: Option<&str>) -> Result<Self> {
        let config = effective_worktree_config(git_dir, parameters_env)?;
        let threshold = match config.get("core", None, "bigfilethreshold") {
            Some(value) => match sley_config::parse_config_int(value) {
                Some(value) if value >= 0 => value as u64,
                _ => {
                    eprintln!(
                        "fatal: bad numeric config value '{value}' for 'core.bigfilethreshold': invalid unit"
                    );
                    return Err(GitError::Exit(128));
                }
            },
            None => 512 * 1024 * 1024,
        };
        let compression_level = pack_compression_level(&config);
        let pack_size_limit = config
            .get("pack", None, "packSizeLimit")
            .and_then(sley_config::parse_config_int)
            .and_then(|value| (value > 0).then_some(value as u64));
        Ok(Self {
            threshold,
            compression_level,
            pack_size_limit,
        })
    }
}

pub(crate) fn effective_worktree_config(
    git_dir: &Path,
    parameters_env: Option<&str>,
) -> Result<GitConfig> {
    let common = common_git_dir_for_worktree_config(git_dir);
    let context = sley_config::ConfigIncludeContext::new(
        Some(common.clone()),
        sley_config::repo_current_branch_name(git_dir),
    );
    let mut config = sley_config::load_effective_config(&common, &context)?;
    if let Ok(parameters) = sley_config::injected_config_parameters(parameters_env) {
        let base = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        sley_config::append_injected_config_sections_with_includes(
            &mut config,
            &parameters,
            &context,
            &base,
        )?;
    }
    Ok(config)
}

pub(crate) fn common_git_dir_for_worktree_config(git_dir: &Path) -> PathBuf {
    if let Ok(value) = fs::read_to_string(git_dir.join("commondir")) {
        let path = PathBuf::from(value.trim());
        if path.is_absolute() {
            return path;
        }
        return git_dir.join(path);
    }
    git_dir.to_path_buf()
}

pub(crate) fn pack_compression_level(config: &GitConfig) -> u32 {
    config_int_in_range(config.get("pack", None, "compression"))
        .or_else(|| config_int_in_range(config.get("core", None, "compression")))
        .unwrap_or(6)
}

pub(crate) fn config_int_in_range(value: Option<&str>) -> Option<u32> {
    let parsed = sley_config::parse_config_int(value?)?;
    (0..=9).contains(&parsed).then_some(parsed as u32)
}

/// A single positional path passed to `update-index`, together with the
/// *mode* that was active at the point the path was seen on the command line.
///
/// git's `update-index` processes argv left-to-right with `parse_options_step`
/// (`PARSE_OPT_STOP_AT_NON_OPTION`): the mode flags `--add`/`--remove`/
/// `--force-remove`/`--info-only`/`--chmod` set sticky global state, and each
/// non-option path is handed to `update_one()` under whatever state is in
/// effect *at that point*. So `--add foo --force-remove bar` ADDs `foo` and
/// FORCE-REMOVEs `bar` — the flags are positional, not global. We mirror that
/// by snapshotting the mode onto each path as it is parsed, rather than
/// applying one batch-wide `UpdateIndexOptions` to every path.
///
/// `--chmod=(+|-)x` is likewise sticky (`--chmod=+x A --chmod=-x B` flips A
/// executable and B non-executable). Each path reports its action
/// (`add '<p>'`, `remove '<p>'`, `chmod (+|-)x '<p>'`) inline under `--verbose`,
/// interleaved in command-line order — which is why the mode must travel with
/// the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateIndexPathMode {
    pub add: bool,
    pub remove: bool,
    pub force_remove: bool,
    pub info_only: bool,
    /// `--chmod=+x` → `Some(true)`, `--chmod=-x` → `Some(false)`, else `None`.
    pub chmod: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct UpdateIndexPath {
    pub path: PathBuf,
    pub mode: UpdateIndexPathMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WriteTreeOptions {
    pub missing_ok: bool,
    pub prefix: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortStatusEntry {
    pub index: u8,
    pub worktree: u8,
    pub path: Vec<u8>,
    pub head_mode: Option<u32>,
    pub index_mode: Option<u32>,
    pub worktree_mode: Option<u32>,
    pub head_oid: Option<ObjectId>,
    pub index_oid: Option<ObjectId>,
    /// For a tracked gitlink (submodule) path: how the submodule's working
    /// state differs from the staged gitlink. `None` for ordinary paths.
    pub submodule: Option<SubmoduleStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortStatusRow<'a> {
    pub index: u8,
    pub worktree: u8,
    pub path: &'a [u8],
    pub head_mode: Option<u32>,
    pub index_mode: Option<u32>,
    pub worktree_mode: Option<u32>,
    pub head_oid: Option<ObjectId>,
    pub index_oid: Option<ObjectId>,
    /// For a tracked gitlink (submodule) path: how the submodule's working
    /// state differs from the staged gitlink. `None` for ordinary paths.
    pub submodule: Option<SubmoduleStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamControl {
    #[default]
    Continue,
    Stop,
}

impl StreamControl {
    pub(crate) fn is_stop(self) -> bool {
        matches!(self, Self::Stop)
    }
}

/// Submodule-specific change detail for a status entry, mirroring upstream's
/// `wt_status_change_data` trio: `new_submodule_commits` plus the
/// `DIRTY_SUBMODULE_MODIFIED`/`DIRTY_SUBMODULE_UNTRACKED` dirty bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubmoduleStatus {
    /// The submodule's checked-out HEAD differs from the staged gitlink oid.
    pub new_commits: bool,
    /// The submodule has staged or unstaged changes to tracked files.
    pub modified_content: bool,
    /// The submodule has untracked files.
    pub untracked_content: bool,
}

impl SubmoduleStatus {
    pub fn any(&self) -> bool {
        self.new_commits || self.modified_content || self.untracked_content
    }
}

/// Bit set in a submodule dirt mask when the submodule has staged or unstaged
/// changes to tracked files (upstream `DIRTY_SUBMODULE_MODIFIED`).
pub const DIRTY_SUBMODULE_MODIFIED: u8 = 1;
/// Bit set in a submodule dirt mask when the submodule has untracked files
/// (upstream `DIRTY_SUBMODULE_UNTRACKED`).
pub const DIRTY_SUBMODULE_UNTRACKED: u8 = 2;

/// Inspect the working state of the submodule whose worktree is at `sub_root`
/// and report its dirt mask: [`DIRTY_SUBMODULE_MODIFIED`] for staged/unstaged
/// changes to tracked files, [`DIRTY_SUBMODULE_UNTRACKED`] for untracked
/// files. Returns 0 for a clean submodule — and for a directory that is not a
/// populated repository at all (upstream treats an unpopulated gitlink as
/// always unchanged). The native equivalent of upstream's
/// `is_submodule_modified()` (which runs `git status --porcelain=2` inside the
/// submodule and classifies `?` lines as untracked, everything else as
/// modified).
pub fn submodule_dirt(sub_root: &Path) -> u8 {
    let Some(git_dir) = sley_diff_merge::gitlink_git_dir(sub_root) else {
        return 0;
    };
    let Ok(config) = sley_config::read_repo_config(&git_dir, None) else {
        return 0;
    };
    let Ok(format) = config.repository_object_format() else {
        return 0;
    };
    let mut dirt = 0;
    let status_result = stream_short_status_with_options(
        sub_root,
        &git_dir,
        format,
        ShortStatusOptions {
            include_ignored: false,
            ignored_mode: StatusIgnoredMode::Traditional,
            untracked_mode: StatusUntrackedMode::Normal,
        },
        |entry| {
            if let Some(submodule) = entry.submodule {
                if submodule.new_commits || submodule.modified_content {
                    dirt |= DIRTY_SUBMODULE_MODIFIED;
                }
                if submodule.untracked_content {
                    dirt |= DIRTY_SUBMODULE_UNTRACKED;
                }
            } else if entry.index == b'?' && entry.worktree == b'?' {
                dirt |= DIRTY_SUBMODULE_UNTRACKED;
            } else {
                dirt |= DIRTY_SUBMODULE_MODIFIED;
            }
            let complete = DIRTY_SUBMODULE_MODIFIED | DIRTY_SUBMODULE_UNTRACKED;
            Ok(if dirt == complete {
                StreamControl::Stop
            } else {
                StreamControl::Continue
            })
        },
    );
    if status_result.is_err() {
        return 0;
    }
    dirt
}

/// As [`submodule_dirt`], but fail the way git does when the gitlink is
/// *populated yet broken* — a `.git` file whose `gitdir:` pointer is gone (the
/// submodule's git directory was moved/deleted). git's `status` / `diff-index`
/// die with "fatal: not a git repository: …" in this case; an unpopulated
/// gitlink (no `.git` at all) stays clean. Used on the status paths so
/// `git status` and `git describe --dirty` surface the breakage (and
/// `--broken` can tolerate it).
pub fn submodule_dirt_checked(sub_root: &Path) -> Result<u8> {
    if let Some(target) = sley_diff_merge::gitlink_broken_gitdir(sub_root) {
        eprintln!("fatal: not a git repository: {}", target.display());
        return Err(GitError::Exit(128));
    }
    Ok(submodule_dirt(sub_root))
}

pub(crate) fn embedded_repo_object_format(sub_root: &Path) -> Option<ObjectFormat> {
    let git_dir = sley_diff_merge::gitlink_git_dir(sub_root)?;
    sley_config::read_repo_config(&git_dir, None)
        .ok()?
        .repository_object_format()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusUntrackedMode {
    #[default]
    All,
    Normal,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusIgnoredMode {
    #[default]
    Traditional,
    Matching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShortStatusOptions {
    pub include_ignored: bool,
    pub ignored_mode: StatusIgnoredMode,
    pub untracked_mode: StatusUntrackedMode,
}

/// The worktree state of one tracked path relative to an expected index/tree
/// entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeEntryState {
    /// The path exists in the worktree and matches the expected mode/object id.
    Clean,
    /// The path exists, but its type, mode, filtered content, symlink target, or
    /// gitlink HEAD differs from the expected entry.
    Modified,
    /// The path, or one of its parents, is missing from the worktree.
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AtomicMetadataWriteOptions {
    pub fsync_file: bool,
    pub fsync_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicMetadataWriteResult {
    pub path: PathBuf,
    pub len: u64,
    pub mtime: Option<(u64, u64)>,
}

/// Stage-0 index stat data that can prove a worktree path clean without
/// re-reading and re-hashing it.
///
/// This is the public carrier for sley's racy-git shortcut. Callers that already
/// parsed `.git/index` can build a probe from the matching [`IndexEntry`] and
/// the index file's mtime, then pass it to [`worktree_entry_state`] or
/// [`worktree_entry_state_by_git_path`]. The probe is trusted only when its path,
/// mode, and object id match the expected entry and the cached stat is not
/// racily clean; otherwise the helper falls back to the same content hashing
/// path used by [`stream_short_status_with_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatProbe {
    entry: IndexEntry,
    index_mtime: Option<(u64, u64)>,
}

/// Reusable stage-0 index stat probes for many worktree paths.
///
/// Prefer this over repeated [`IndexStatProbe::from_repository_index`] calls
/// when an embedder needs to verify many paths. It parses `.git/index` once,
/// records the index file mtime used for racy-git checks, and serves cheap
/// per-path probes from memory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexStatProbeCache {
    entries: HashMap<Vec<u8>, IndexEntry>,
    index_mtime: Option<(u64, u64)>,
}

impl IndexStatProbe {
    /// Build a probe from a parsed stage-0 index entry and the index file's mtime
    /// split as `(seconds, nanoseconds)`.
    pub fn from_index_entry(entry: IndexEntry, index_mtime: Option<(u64, u64)>) -> Self {
        Self { entry, index_mtime }
    }

    /// Build a probe from a parsed index entry and the path of the index file on
    /// disk, using that file's mtime as the racy-clean reference timestamp.
    pub fn from_index_entry_and_index_path(
        entry: IndexEntry,
        index_path: impl AsRef<Path>,
    ) -> Self {
        let index_mtime = fs::metadata(index_path.as_ref())
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        Self { entry, index_mtime }
    }

    /// Read this repository's index and return a probe for `git_path` when a
    /// stage-0 entry exists.
    ///
    /// For repeated lookups prefer [`IndexStatProbeCache::from_repository_index`]
    /// and [`IndexStatProbeCache::probe_for_git_path`]. This one-shot helper
    /// keeps a small process-local cache for back-to-back calls against an
    /// unchanged index, but the explicit cache makes ownership and invalidation
    /// clearer for high-volume embedders.
    pub fn from_repository_index(
        git_dir: impl AsRef<Path>,
        format: ObjectFormat,
        git_path: &[u8],
    ) -> Result<Option<Self>> {
        let index_path = repository_index_path(git_dir);
        cached_repository_index_stat_probe(&index_path, format, git_path)
    }

    /// The parsed index entry this probe was built from.
    pub fn entry(&self) -> &IndexEntry {
        &self.entry
    }

    /// The index file mtime used as the racy-clean reference timestamp.
    pub fn index_mtime(&self) -> Option<(u64, u64)> {
        self.index_mtime
    }

    pub(crate) fn stat_cache_for(
        &self,
        git_path: &[u8],
        expected_oid: &ObjectId,
        expected_mode: u32,
    ) -> Option<IndexStatCache> {
        if index_entry_stage(&self.entry) != 0
            || self.entry.path.as_bytes() != git_path
            || self.entry.oid != *expected_oid
            || self.entry.mode != expected_mode
        {
            return None;
        }
        let mut entries = HashMap::new();
        entries.insert(git_path.to_vec(), self.entry.clone());
        Some(IndexStatCache {
            entries,
            index_mtime: self.index_mtime,
        })
    }
}

impl IndexStatProbeCache {
    /// Build a reusable probe cache from an already parsed index and index-file
    /// mtime.
    pub fn from_index(index: &Index, index_mtime: Option<(u64, u64)>) -> Self {
        Self {
            entries: stage0_index_entries(index),
            index_mtime,
        }
    }

    /// Read this repository's index once and build reusable stat probes.
    ///
    /// A missing index returns an empty cache, matching the one-shot helper's
    /// `Ok(None)` result for every path.
    pub fn from_repository_index(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Result<Self> {
        let index_path = repository_index_path(git_dir);
        read_index_stat_probe_cache(&index_path, format)
    }

    /// Return a per-path probe for a stage-0 entry, if present.
    pub fn probe_for_git_path(&self, git_path: &[u8]) -> Option<IndexStatProbe> {
        self.entries
            .get(git_path)
            .cloned()
            .map(|entry| IndexStatProbe {
                entry,
                index_mtime: self.index_mtime,
            })
    }

    /// Whether this cache has a stage-0 entry for `git_path`.
    pub fn contains_git_path(&self, git_path: &[u8]) -> bool {
        self.entries.contains_key(git_path)
    }

    /// Number of stage-0 entries in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache has no stage-0 entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index file mtime used as the racy-clean reference timestamp.
    pub fn index_mtime(&self) -> Option<(u64, u64)> {
        self.index_mtime
    }
}

#[derive(Clone)]
pub(crate) struct CachedRepositoryIndexStatProbes {
    index_path: PathBuf,
    format: ObjectFormat,
    len: u64,
    mtime: Option<(u64, u64)>,
    probes: IndexStatProbeCache,
}

pub(crate) static REPOSITORY_INDEX_STAT_PROBES: OnceLock<
    Mutex<Option<CachedRepositoryIndexStatProbes>>,
> = OnceLock::new();

pub(crate) fn cached_repository_index_stat_probe(
    index_path: &Path,
    format: ObjectFormat,
    git_path: &[u8],
) -> Result<Option<IndexStatProbe>> {
    let metadata = match fs::metadata(index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(cache) = REPOSITORY_INDEX_STAT_PROBES.get()
                && let Ok(mut guard) = cache.lock()
            {
                *guard = None;
            }
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    let len = metadata.len();
    let mtime = file_mtime_parts(&metadata);
    let cache = REPOSITORY_INDEX_STAT_PROBES.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.as_ref()
        && cached.index_path == index_path
        && cached.format == format
        && cached.len == len
        && cached.mtime == mtime
    {
        return Ok(cached.probes.probe_for_git_path(git_path));
    }

    let probes = read_index_stat_probe_cache_with_metadata(index_path, format, mtime)?;
    let probe = probes.probe_for_git_path(git_path);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedRepositoryIndexStatProbes {
            index_path: index_path.to_path_buf(),
            format,
            len,
            mtime,
            probes: probes.clone(),
        });
    }
    Ok(probe)
}

pub(crate) fn read_index_stat_probe_cache(
    index_path: &Path,
    format: ObjectFormat,
) -> Result<IndexStatProbeCache> {
    let metadata = match fs::metadata(index_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(IndexStatProbeCache::default());
        }
        Err(err) => return Err(err.into()),
    };
    read_index_stat_probe_cache_with_metadata(index_path, format, file_mtime_parts(&metadata))
}

pub(crate) fn read_index_stat_probe_cache_with_metadata(
    index_path: &Path,
    format: ObjectFormat,
    index_mtime: Option<(u64, u64)>,
) -> Result<IndexStatProbeCache> {
    let bytes = fs::read(index_path)?;
    let index = Index::parse(&bytes, format)?;
    Ok(IndexStatProbeCache::from_index(&index, index_mtime))
}

pub(crate) fn stage0_index_entries(index: &Index) -> HashMap<Vec<u8>, IndexEntry> {
    let mut entries = HashMap::new();
    for entry in &index.entries {
        if index_entry_stage(entry) == 0 {
            entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        }
    }
    entries
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutResult {
    pub branch: String,
    pub oid: ObjectId,
    pub files: usize,
}

/// Typed local-change report produced after a branch checkout.
///
/// This is independent of the index's full or sparse representation; callers
/// render the same entries for ordinary, sparse-checkout, and sparse-index
/// repositories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutChangeSummary {
    pub changes: Vec<sley_diff_merge::NameStatusEntry>,
}

/// Deterministic worker selection for checkout materialization.
///
/// A zero `worker_count` means the caller should stay sequential.  Otherwise
/// exactly `worker_count` workers consume a shared queue; path-conflicting
/// entries are serialized by the materializer while independent top-level
/// paths may be written concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParallelCheckoutPlan {
    pub worker_count: usize,
    pub item_count: usize,
    pub threshold: usize,
}

/// One path that could not be restored during an otherwise partially
/// successful index checkout.
#[derive(Debug)]
pub struct CheckoutPathFailure {
    pub path: Vec<u8>,
    pub error: GitError,
}

/// Typed result of a path checkout after every independent entry and delayed
/// filter has had a chance to complete.
#[derive(Debug)]
pub struct CheckoutIndexPathOutcome {
    pub restored: usize,
    pub failures: Vec<CheckoutPathFailure>,
}

/// One independent blob/symlink checkout requested by an unpack-trees
/// consumer. Gitlinks remain with the caller because recursive submodule
/// movement has repository-level semantics beyond file materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutMaterializationEntry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub oid: ObjectId,
}

/// Stat data produced by a completed checkout materialization batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutMaterializationOutcome {
    pub stats: BTreeMap<Vec<u8>, Option<sley_unpack_trees::StatInfo>>,
}

impl ParallelCheckoutPlan {
    pub fn from_config(config: &GitConfig, item_count: usize) -> Self {
        let requested = config
            .get("checkout", None, "workers")
            .and_then(sley_config::parse_config_int)
            .unwrap_or(1);
        let threshold = config
            .get("checkout", None, "thresholdForParallelism")
            .and_then(sley_config::parse_config_int)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(100);
        let available = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        let requested = match requested {
            0 => available,
            value if value > 0 => usize::try_from(value).unwrap_or(usize::MAX),
            _ => 1,
        };
        let worker_count = if requested > 1 && item_count >= threshold && item_count > 1 {
            requested.min(item_count)
        } else {
            0
        };
        Self {
            worker_count,
            item_count,
            threshold,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreResult {
    pub restored: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutStage {
    Ours,
    Theirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutConflictStyle {
    Merge,
    Diff3,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckoutIndexPathOptions<'a> {
    pub force: bool,
    pub merge: bool,
    pub overlay: bool,
    pub stage: Option<CheckoutStage>,
    pub conflict_style: CheckoutConflictStyle,
    pub smudge_config: Option<&'a GitConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveResult {
    pub removed: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveResult {
    pub source: Vec<u8>,
    pub destination: Vec<u8>,
    pub skipped: bool,
    pub fatal: Option<String>,
    pub details: Vec<MoveDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveDetail {
    pub source: Vec<u8>,
    pub destination: Vec<u8>,
    pub skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitmodulesMove {
    pub(crate) source: Vec<u8>,
    pub(crate) destination: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitlinkGitdirMove {
    pub(crate) git_dir: PathBuf,
    pub(crate) destination_root: PathBuf,
}

pub fn repository_index_path(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.as_ref().join("index"))
}

pub fn read_repository_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Option<Index>> {
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(None);
    }
    if fs::metadata(&index_path)
        .map(|metadata| metadata.len() == 0)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    Ok(Some(sley_index::read_repository_index(git_dir, format)?))
}

pub(crate) fn empty_index() -> Index {
    Index {
        version: 2,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    }
}

/// Resolve the working-tree root for a repository identified by its git
/// directory, returning `Ok(None)` for a bare repository.
///
/// This is the repository-intrinsic worktree resolution (it does *not* consult
/// `GIT_WORK_TREE`/`GIT_DIR` or CLI overrides — those are the caller's job):
///
/// 0. for a linked worktree (a git directory that has both a `commondir` and a
///    `gitdir` administrative file), the directory containing the worktree's
///    `.git` link, canonicalised;
/// 1. otherwise, if `core.bare` is true the repository is bare and `Ok(None)` is
///    returned immediately — `core.bare` takes precedence for the main repo, so
///    a bare repo ignores `core.worktree` and the `.git`-parent fallback;
/// 2. otherwise, a `core.worktree` setting in `<git_dir>/config` (absolute, or
///    relative to the git directory), canonicalised;
/// 3. otherwise, when the git directory is a `.git` directory, its parent (the
///    ordinary non-bare layout) — returned verbatim, not canonicalised;
/// 4. otherwise the repository is bare and `Ok(None)` is returned.
///
/// `Ok(None)` means specifically "bare" (case 0 or case 4). A [`GitError::Io`] is
/// returned if a path that should exist cannot be canonicalised, and a
/// [`GitError::InvalidPath`] if a `.git` directory has no parent (a malformed
/// layout).
pub fn worktree_root_for_git_dir(git_dir: &Path) -> Result<Option<PathBuf>> {
    if git_dir.join("commondir").is_file() {
        let gitdir_file = git_dir.join("gitdir");
        if gitdir_file.is_file() {
            let value = fs::read_to_string(&gitdir_file)?;
            let worktree_git_file = resolve_worktree_admin_path(git_dir, value.trim());
            if let Some(worktree) = worktree_git_file.parent() {
                return fs::canonicalize(worktree)
                    .map(Some)
                    .map_err(|err| GitError::Io(err.to_string()));
            }
        }
    }
    if let Ok(config) = sley_config::read_repo_config(git_dir, None) {
        // A bare repository has no working tree, and `core.bare` takes precedence:
        // a bare repo ignores `core.worktree`. Check it before any worktree
        // resolution so a bare `.git`-named directory does not fall through to the
        // "parent of .git" case below.
        if config.get_bool("core", None, "bare") == Some(true) {
            return Ok(None);
        }
        if let Some(worktree) = config.get("core", None, "worktree") {
            let worktree = PathBuf::from(worktree);
            let worktree = if worktree.is_absolute() {
                worktree
            } else {
                git_dir.join(worktree)
            };
            return fs::canonicalize(worktree)
                .map(Some)
                .map_err(|err| GitError::Io(err.to_string()));
        }
    }
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return Ok(None);
    }
    git_dir
        .parent()
        .map(Path::to_path_buf)
        .map(Some)
        .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()))
}

pub fn common_git_dir_for_git_dir(git_dir: &Path) -> Result<PathBuf> {
    if let Some(common_dir) = env::var_os("GIT_COMMON_DIR") {
        return Ok(PathBuf::from(common_dir));
    }
    let commondir = git_dir.join("commondir");
    if commondir.is_file() {
        let value = fs::read_to_string(&commondir)?;
        let path = PathBuf::from(value.trim());
        let common = if path.is_absolute() {
            path
        } else {
            git_dir.join(path)
        };
        return fs::canonicalize(common).map_err(|err| GitError::Io(err.to_string()));
    }
    fs::canonicalize(git_dir).map_err(|err| GitError::Io(err.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSymrefWorktree {
    pub refname: String,
    pub path: PathBuf,
}

pub(crate) struct WorktreeAdmin {
    git_dir: PathBuf,
    path: Option<PathBuf>,
}

/// If `refname` is listed in any in-progress rebase's `update-refs` file,
/// return the worktree that owns that rebase (mirrors git's treatment of those
/// refs as checked-out for `branch -f` / `worktree add` guards).
pub fn worktree_holding_rebase_update_ref(
    git_dir: &Path,
    refname: &str,
) -> Result<Option<SharedSymrefWorktree>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for admin in worktree_admins(&common_git_dir)? {
        let Some(path) = admin.path.clone() else {
            continue;
        };
        if worktree_rebase_update_refs(&admin.git_dir)
            .iter()
            .any(|name| name == refname)
        {
            return Ok(Some(SharedSymrefWorktree {
                refname: refname.to_string(),
                path,
            }));
        }
    }
    Ok(None)
}

pub fn find_shared_symref(
    git_dir: &Path,
    symref: &str,
    target: &str,
) -> Result<Option<SharedSymrefWorktree>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    for admin in worktree_admins(&common_git_dir)? {
        // git's `is_shared_symref` returns 0 for a bare worktree: a bare main
        // repo has no working tree, so its `HEAD` branch is never "checked out"
        // and must not block updates (t5516 "… bare repository worktree").
        let Some(path) = admin.path.clone() else {
            continue;
        };
        if worktree_uses_symref(&admin.git_dir, symref, target)? {
            return Ok(Some(SharedSymrefWorktree {
                refname: target.to_string(),
                path,
            }));
        }
    }
    Ok(None)
}

pub fn worktree_refs_in_use(git_dir: &Path) -> Result<HashSet<String>> {
    let common_git_dir = common_git_dir_for_git_dir(git_dir)?;
    let mut refs = HashSet::new();
    for admin in worktree_admins(&common_git_dir)? {
        if let Ok(head) = fs::read_to_string(admin.git_dir.join("HEAD")) {
            let head = head.trim();
            if let Some(target) = head.strip_prefix("ref: ") {
                refs.insert(target.to_string());
            }
            refs.extend(worktree_detached_operation_refs(&admin.git_dir));
        }
    }
    Ok(refs)
}

pub(crate) fn worktree_admins(common_git_dir: &Path) -> Result<Vec<WorktreeAdmin>> {
    let mut admins = Vec::new();
    admins.push(WorktreeAdmin {
        git_dir: common_git_dir.to_path_buf(),
        path: worktree_root_for_git_dir(common_git_dir)?,
    });
    let worktrees_dir = common_git_dir.join("worktrees");
    let Ok(entries) = fs::read_dir(worktrees_dir) else {
        return Ok(admins);
    };
    for entry in entries {
        let entry = entry?;
        let git_dir = entry.path();
        let path = linked_worktree_path(&git_dir);
        admins.push(WorktreeAdmin { git_dir, path });
    }
    Ok(admins)
}

pub(crate) fn linked_worktree_path(admin_dir: &Path) -> Option<PathBuf> {
    let gitdir = fs::read_to_string(admin_dir.join("gitdir")).ok()?;
    let gitdir = gitdir.trim();
    if gitdir.is_empty() {
        return None;
    }
    let gitdir_path = resolve_worktree_admin_path(admin_dir, gitdir);
    gitdir_path.parent().map(|path| {
        fs::canonicalize(path).unwrap_or_else(|_| normalize_lexical_worktree_path(path))
    })
}

pub(crate) fn normalize_lexical_worktree_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

pub(crate) fn worktree_uses_symref(git_dir: &Path, symref: &str, target: &str) -> Result<bool> {
    if symref != "HEAD" {
        return Ok(false);
    }
    if worktree_head_symref_target(&git_dir.join(symref)).as_deref() == Some(target) {
        return Ok(true);
    }
    if worktree_rebase_update_refs(git_dir)
        .iter()
        .any(|name| name == target)
    {
        return Ok(true);
    }
    if worktree_detached_operation_uses_ref(git_dir, target) {
        return Ok(true);
    }
    Ok(false)
}

fn worktree_head_symref_target(path: &Path) -> Option<String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
        && let Ok(target) = fs::read_link(path)
    {
        let target = target.to_string_lossy();
        if target.starts_with("refs/") {
            return Some(target.into_owned());
        }
    }
    let head = fs::read_to_string(path).ok()?;
    head.trim().strip_prefix("ref: ").map(str::to_string)
}

pub(crate) fn worktree_detached_operation_uses_ref(git_dir: &Path, target: &str) -> bool {
    worktree_detached_operation_refs(git_dir)
        .iter()
        .any(|name| name == target)
}

pub(crate) fn worktree_detached_operation_refs(git_dir: &Path) -> Vec<String> {
    let mut refs = Vec::new();
    for dir in ["rebase-merge", "rebase-apply"] {
        let Some(refname) = operation_head_name_ref(git_dir.join(dir).join("head-name")) else {
            continue;
        };
        refs.push(refname);
    }
    refs.extend(worktree_rebase_update_refs(git_dir));
    if let Some(refname) = operation_head_name_ref(git_dir.join("BISECT_START")) {
        refs.push(refname);
    }
    refs
}

pub(crate) fn worktree_rebase_update_refs(git_dir: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(git_dir.join("rebase-merge").join("update-refs")) else {
        return Vec::new();
    };
    text.lines()
        .step_by(3)
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| line.to_string())
        })
        .collect()
}

pub(crate) fn operation_head_name_ref(path: PathBuf) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("refs/heads/") {
        Some(value.to_string())
    } else {
        Some(format!("refs/heads/{value}"))
    }
}

/// Resolve a path read from a git-directory administrative file (e.g. the
/// `gitdir` link of a linked worktree): absolute paths are kept as-is, relative
/// paths are joined onto the administrative directory.
pub(crate) fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        admin_dir.join(path)
    }
}

/// Whether the repository at `git_dir` is shallow — i.e. it has a `shallow`
/// file recording grafted commit boundaries (`git clone --depth`).
pub fn is_shallow_repository(git_dir: &Path) -> bool {
    git_dir.join("shallow").exists()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub cached: bool,
    pub force: bool,
    pub dry_run: bool,
    pub ignore_unmatch: bool,
    /// `git rm --sparse`: permit pathspecs to select skip-worktree entries and
    /// expand a collapsed sparse-directory entry when leaf selection requires
    /// it. Without this opt-in, out-of-cone matches are rejected.
    pub sparse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveOptions {
    pub force: bool,
    pub dry_run: bool,
    pub skip_errors: bool,
    /// `git mv --sparse`: allow moving entries into or out of the
    /// sparse-checkout, reconciling each moved entry's skip-worktree bit and
    /// worktree materialization with the destination's cone membership.
    pub sparse: bool,
}

impl ShortStatusEntry {
    pub fn as_row(&self) -> ShortStatusRow<'_> {
        ShortStatusRow {
            index: self.index,
            worktree: self.worktree,
            path: &self.path,
            head_mode: self.head_mode,
            index_mode: self.index_mode,
            worktree_mode: self.worktree_mode,
            head_oid: self.head_oid,
            index_oid: self.index_oid,
            submodule: self.submodule,
        }
    }

    pub fn line(&self) -> String {
        format!(
            "{}{} {}",
            self.index as char,
            self.worktree as char,
            String::from_utf8_lossy(&self.path)
        )
    }
}

impl ShortStatusRow<'_> {
    pub fn to_owned_entry(self) -> ShortStatusEntry {
        ShortStatusEntry {
            index: self.index,
            worktree: self.worktree,
            path: self.path.to_vec(),
            head_mode: self.head_mode,
            index_mode: self.index_mode,
            worktree_mode: self.worktree_mode,
            head_oid: self.head_oid,
            index_oid: self.index_oid,
            submodule: self.submodule,
        }
    }

    pub fn line(&self) -> String {
        format!(
            "{}{} {}",
            self.index as char,
            self.worktree as char,
            String::from_utf8_lossy(self.path)
        )
    }
}
