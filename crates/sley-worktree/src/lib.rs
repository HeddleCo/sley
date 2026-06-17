use sley_config::GitConfig;
use sley_core::{
    BString, GitError, MissingObjectContext, MissingObjectKind, ObjectFormat, ObjectId, RepoPath,
    Result,
};
use sley_index::{
    BorrowedIndex, CacheTree, Index, IndexEntry, IndexEntryRef, SPARSE_DIR_MODE, Stage,
};
use sley_object::{Commit, EncodedObject, ObjectType, Tree, TreeEntry, tree_entry_object_type};
use sley_odb::{FileObjectDatabase, ObjectPresenceChecker, ObjectReader, ObjectWriter};
use sley_refs::{FileRefStore, RefTarget, RefUpdate, ReflogEntry, branch_ref_name};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, UNIX_EPOCH};
use std::{env, fs};

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
}

impl UpdateIndexOptions {
    /// The uniform per-path mode this batch applies to every path.
    fn path_mode(&self) -> UpdateIndexPathMode {
        UpdateIndexPathMode {
            add: self.add,
            remove: self.remove,
            force_remove: self.force_remove,
            info_only: self.info_only,
            chmod: self.chmod,
        }
    }
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
    fn is_stop(self) -> bool {
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
            if entry.index == b'?' && entry.worktree == b'?' {
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

    fn stat_cache_for(
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
struct CachedRepositoryIndexStatProbes {
    index_path: PathBuf,
    format: ObjectFormat,
    len: u64,
    mtime: Option<(u64, u64)>,
    probes: IndexStatProbeCache,
}

static REPOSITORY_INDEX_STAT_PROBES: OnceLock<Mutex<Option<CachedRepositoryIndexStatProbes>>> =
    OnceLock::new();

fn cached_repository_index_stat_probe(
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

fn read_index_stat_probe_cache(
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

fn read_index_stat_probe_cache_with_metadata(
    index_path: &Path,
    format: ObjectFormat,
    index_mtime: Option<(u64, u64)>,
) -> Result<IndexStatProbeCache> {
    let bytes = fs::read(index_path)?;
    let index = Index::parse(&bytes, format)?;
    Ok(IndexStatProbeCache::from_index(&index, index_mtime))
}

fn stage0_index_entries(index: &Index) -> HashMap<Vec<u8>, IndexEntry> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreResult {
    pub restored: usize,
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

pub fn repository_index_path(git_dir: impl AsRef<Path>) -> PathBuf {
    env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.as_ref().join("index"))
}

pub fn read_repository_index(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Option<Index>> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(None);
    }
    Ok(Some(Index::parse(&fs::read(index_path)?, format)?))
}

fn empty_index() -> Index {
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
/// 0. if `core.bare` is true the repository is bare and `Ok(None)` is returned
///    immediately — `core.bare` takes precedence, so a bare repo ignores
///    `core.worktree` and the `.git`-parent fallback;
/// 1. otherwise, a `core.worktree` setting in `<git_dir>/config` (absolute, or
///    relative to the git directory), canonicalised;
/// 2. otherwise, for a linked worktree (a git directory that has both a
///    `commondir` and a `gitdir` administrative file), the directory containing
///    the worktree's `.git` link, canonicalised;
/// 3. otherwise, when the git directory is a `.git` directory, its parent (the
///    ordinary non-bare layout) — returned verbatim, not canonicalised;
/// 4. otherwise the repository is bare and `Ok(None)` is returned.
///
/// `Ok(None)` means specifically "bare" (case 0 or case 4). A [`GitError::Io`] is
/// returned if a path that should exist cannot be canonicalised, and a
/// [`GitError::InvalidPath`] if a `.git` directory has no parent (a malformed
/// layout).
pub fn worktree_root_for_git_dir(git_dir: &Path) -> Result<Option<PathBuf>> {
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
    if git_dir.file_name().and_then(|name| name.to_str()) != Some(".git") {
        return Ok(None);
    }
    git_dir
        .parent()
        .map(Path::to_path_buf)
        .map(Some)
        .ok_or_else(|| GitError::InvalidPath("git dir has no parent worktree".into()))
}

/// Resolve a path read from a git-directory administrative file (e.g. the
/// `gitdir` link of a linked worktree): absolute paths are kept as-is, relative
/// paths are joined onto the administrative directory.
fn resolve_worktree_admin_path(admin_dir: &Path, value: &str) -> PathBuf {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveOptions {
    pub force: bool,
    pub dry_run: bool,
    pub skip_errors: bool,
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
    let index = read_repository_index(git_dir, format)?.unwrap_or_else(empty_index);
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
    )
}

/// Stamp a single uniform mode (from a batch-wide [`UpdateIndexOptions`]) onto
/// every path. Used by the `git add`-style callers that genuinely apply one
/// mode to all paths; the positional `git update-index <flag> <path>...` path
/// instead snapshots a distinct mode per path in the CLI parse walk.
fn ordered_paths_from_plain(
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
    let index = read_repository_index(git_dir, format)?.unwrap_or_else(empty_index);
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
    let index = read_repository_index(git_dir, format)?.unwrap_or_else(empty_index);
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
    let prechecks = tracked_only_non_clean_prechecks_parallel(worktree_root, &index, &stat_cache)?;
    if prechecks.is_empty() {
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

    if index_dirty {
        normalize_index_version_for_extended_flags(&mut index);
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
        fs::write(index_path, index.write(format)?)?;
    }
    Ok(actions)
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
    if entry.stage() != Stage::Normal || index_entry_skip_worktree(&entry) || sley_index::is_gitlink(entry.mode)
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
        // git's index update folds in `global_conv_flags_eol`, so `git add`
        // emits the `core.safecrlf` round-trip warning (default: warn). The
        // current index blob (`entry.oid`) drives the auto-crlf
        // `has_crlf_in_index` decision. Mirror the slow `add_update_tracked_path`
        // path here so the exact-patch fast path does not silently drop the
        // warning (upstream t0020 'safecrlf: print warning only once').
        let conv_flags = ConvFlags::from_config(&clean_filter.config);
        let index_blob = match conv_flags {
            ConvFlags::Off => SafeCrlfIndexBlob::None,
            _ => SafeCrlfIndexBlob::Lookup {
                odb: &odb,
                oid: entry.oid,
            },
        };
        apply_clean_filter_with_attributes_cow_safecrlf(
            &clean_filter.config,
            &checks,
            git_path,
            &body,
            conv_flags,
            index_blob,
        )?
        .into_owned()
    };
    let object = EncodedObject::new(ObjectType::Blob, body);
    let oid = object.object_id(format)?;
    if oid != entry.oid {
        odb.write_object(object)?;
    }

    let mut updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
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
    let mut clean_filter = None;
    let (action, dirty) = add_update_tracked_path(
        worktree_root,
        git_dir,
        format,
        None,
        &odb,
        &stat_cache,
        &mut clean_filter,
        &mut index,
        git_path,
    )?;
    if dirty {
        normalize_index_version_for_extended_flags(&mut index);
        index.extensions = index_extensions_without_cache_tree(&index.extensions);
        fs::write(index_path, index.write(format)?)?;
    }
    Ok(action)
}

struct RawExactIndexEntry {
    version: u32,
    entry: IndexEntry,
    entry_start: usize,
    entries_end: usize,
    checksum_offset: usize,
}

fn raw_exact_index_entry(
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

fn raw_exact_entry_can_patch(raw: &RawExactIndexEntry, git_path: &[u8]) -> bool {
    raw.version == 2
        && raw.entry.flags_extended == 0
        && raw.entry.flags & INDEX_FLAG_EXTENDED == 0
        && raw.entry.flags == index_flags(git_path.len(), 0)
        && raw.entry.path.as_bytes() == git_path
}

fn raw_updated_entry_can_patch(
    previous: &IndexEntry,
    updated: &IndexEntry,
    git_path: &[u8],
) -> bool {
    updated.path.as_bytes() == git_path
        && updated.flags_extended == 0
        && updated.flags & INDEX_FLAG_EXTENDED == 0
        && updated.flags == previous.flags
}

fn raw_index_extensions_are_filterable(
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

fn patch_raw_index_entry(
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

fn u32_from_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u16_from_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn add_update_tracked_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    clean_config: Option<&GitConfig>,
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
    if entry.stage() != Stage::Normal {
        return Ok((None, false));
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
        let mut updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
        updated_entry.mode = sley_index::GITLINK_MODE;
        let changed = updated_entry.oid != entry.oid || updated_entry.mode != entry.mode;
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
    if stat_cache.reuse_index_entry(&entry, &metadata).is_some() {
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
        // git's `add -u` index update folds in `global_conv_flags_eol`, so emit
        // the `core.safecrlf` round-trip warning (default: warn). The current
        // index blob (`entry.oid`) drives the auto-crlf `has_crlf_in_index`
        // decision.
        let conv_flags = ConvFlags::from_config(&clean_filter.config);
        let index_blob = match conv_flags {
            ConvFlags::Off => SafeCrlfIndexBlob::None,
            _ => SafeCrlfIndexBlob::Lookup {
                odb,
                oid: entry.oid,
            },
        };
        apply_clean_filter_with_attributes_cow_safecrlf(
            &clean_filter.config,
            &checks,
            git_path,
            &body,
            conv_flags,
            index_blob,
        )?
        .into_owned()
    };
    let object = EncodedObject::new(ObjectType::Blob, body);
    let oid = object.object_id(format)?;
    if oid != entry.oid {
        odb.write_object(object)?;
    }
    let mut updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
    if is_symlink {
        updated_entry.mode = 0o120000;
    }
    let changed = updated_entry.oid != entry.oid || updated_entry.mode != entry.mode;
    if updated_entry != entry {
        replace_index_entries_with_entry(&mut index.entries, updated_entry);
        return Ok((
            changed.then(|| AddUpdateTrackedAction::Add(git_path.to_vec())),
            true,
        ));
    }
    Ok((None, false))
}

enum UpdateIndexCleanFilter {
    Full(AttributeMatcher),
    PathLocal,
}

fn index_entries_path_range(entries: &[IndexEntry], path: &[u8]) -> std::ops::Range<usize> {
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

fn remove_index_entries_with_path(entries: &mut Vec<IndexEntry>, path: &[u8]) -> bool {
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
fn remove_index_entries_under_dir(entries: &mut Vec<IndexEntry>, name: &[u8]) {
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
fn remove_index_dir_name_conflicts(entries: &mut Vec<IndexEntry>, name: &[u8]) {
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

fn replace_index_entries_with_entry(entries: &mut Vec<IndexEntry>, entry: IndexEntry) {
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

fn update_index_paths_impl(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    mut index: Index,
    paths: &[UpdateIndexPath],
    options: UpdateIndexOptions,
    clean_config: Option<&GitConfig>,
    verbose: bool,
) -> Result<UpdateIndexResult> {
    let index_path = repository_index_path(git_dir);
    let odb = FileObjectDatabase::from_git_dir(git_dir, format);
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
    let requested_filter_attrs = filter_attribute_names();
    let mut updated = Vec::new();
    let mut reports: Vec<String> = Vec::new();
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
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if path_mode.force_remove {
            remove_index_entries_with_path(&mut index.entries, &git_path);
            // git's update_one() reports `remove` for a --force-remove path.
            reports.push(format!("remove '{}'", String::from_utf8_lossy(&git_path)));
            continue;
        }
        let existing_range = index_entries_path_range(&index.entries, &git_path);
        if index.entries[existing_range.clone()]
            .iter()
            .any(index_entry_skip_worktree)
        {
            if path_mode.remove && !options.ignore_skip_worktree_entries {
                index.entries.drain(existing_range);
            }
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
        let Some(metadata) = symlink_metadata else {
            if path_mode.remove {
                remove_index_entries_with_path(&mut index.entries, &git_path);
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
            // A directory is stageable only as a gitlink: when it is an
            // embedded repository with a commit checked out, git records a
            // mode-160000 entry whose oid is that commit (no object is
            // written). Otherwise it errors — with upstream's exact messages
            // for the embedded-repo-without-commit and plain-directory cases
            // (object-file.c index_path / builtin/update-index.c
            // process_directory).
            let display = String::from_utf8_lossy(&git_path).into_owned();
            let has_dot_git = absolute.join(".git").exists();
            let Some(head_oid) = sley_diff_merge::gitlink_head_oid(&absolute, format) else {
                if has_dot_git {
                    eprintln!("error: '{display}' does not have a commit checked out");
                } else {
                    eprintln!("error: {display}: is a directory - add files inside instead");
                }
                eprintln!("fatal: Unable to process path {display}");
                return Err(GitError::Exit(128));
            };
            if path_chmod.is_some() {
                eprintln!(
                    "fatal: git update-index: cannot chmod {}x '{display}'",
                    if path_chmod == Some(true) { '+' } else { '-' },
                );
                return Err(GitError::Exit(128));
            }
            let mut entry = index_entry_from_metadata(git_path.clone(), head_oid, &metadata);
            entry.mode = sley_index::GITLINK_MODE;
            reports.push(format!("add '{display}'"));
            replace_index_entries_with_entry(&mut index.entries, entry);
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
            // The safecrlf auto-crlf decision needs the path's *current* index
            // blob (git's `has_crlf_in_index`); the stage-0 entry, if any, has it.
            let index_blob = match conv_flags {
                ConvFlags::Off => SafeCrlfIndexBlob::None,
                _ => stage0_oid_in_range(&index.entries, existing_range.clone()).map_or(
                    SafeCrlfIndexBlob::None,
                    |oid| SafeCrlfIndexBlob::Lookup { odb: &odb, oid },
                ),
            };
            match (clean_config, &clean_filter) {
                (Some(config), Some(UpdateIndexCleanFilter::Full(matcher))) => {
                    // Identical to `apply_clean_filter`, but reuses the batch's
                    // matcher instead of rebuilding it (and re-walking the tree)
                    // for this path.
                    let checks =
                        matcher.attributes_for_path(&git_path, &requested_filter_attrs, false);
                    apply_clean_filter_with_attributes_cow_safecrlf(
                        config, &checks, &git_path, &body, conv_flags, index_blob,
                    )?
                    .into_owned()
                }
                (Some(config), Some(UpdateIndexCleanFilter::PathLocal)) => {
                    let checks = filter_attribute_checks(worktree_root, &git_path)?;
                    apply_clean_filter_with_attributes_cow_safecrlf(
                        config, &checks, &git_path, &body, conv_flags, index_blob,
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
            odb.write_object(object)?
        };
        let mut entry = index_entry_from_metadata(git_path.clone(), oid, &metadata);
        if is_symlink {
            entry.mode = 0o120000;
        }
        // git's update_one() reports `add` for every staged path (whether the
        // entry is new or an update), then chmod_path() reports the chmod after.
        reports.push(format!("add '{}'", String::from_utf8_lossy(&git_path)));
        if let Some(executable) = path_chmod {
            // git's chmod_path() refuses to flip the executable bit on anything
            // that is not a regular file (a symlink/gitlink has no such bit). It
            // writes the blob first, then errors with this exact message and
            // leaves the index untouched.
            if is_symlink {
                eprintln!(
                    "fatal: git update-index: cannot chmod {}x '{}'",
                    if executable { '+' } else { '-' },
                    String::from_utf8_lossy(&git_path)
                );
                return Err(GitError::Exit(128));
            }
            entry.mode = if executable { 0o100755 } else { 0o100644 };
            reports.push(format!(
                "chmod {}x '{}'",
                if executable { '+' } else { '-' },
                String::from_utf8_lossy(&git_path)
            ));
        }
        replace_index_entries_with_entry(&mut index.entries, entry);
        updated.push(oid);
    }
    normalize_index_version_for_extended_flags(&mut index);
    index.extensions = index_extensions_without_cache_tree(&index.extensions);
    fs::write(index_path, index.write(format)?)?;
    if verbose {
        let mut stdout = std::io::stdout().lock();
        for line in &reports {
            writeln!(stdout, "{line}")?;
        }
        stdout.flush()?;
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
            &index_path,
            format,
            index,
            stat_cache,
            quiet,
            ignore_missing,
        );
    }
    let mut needs_update = false;
    let mut index_dirty = false;
    for entry in &mut index.entries {
        if index_entry_stage(entry) != 0 {
            continue;
        }
        let selected_for_update =
            !selected_paths.is_empty() && selected_paths.contains(entry.path.as_bytes());
        if entry.flags & INDEX_FLAG_ASSUME_UNCHANGED != 0 {
            if !really_refresh {
                continue;
            }
            entry.flags &= !INDEX_FLAG_ASSUME_UNCHANGED;
            index_dirty = true;
        }
        let absolute = worktree_root.join(repo_path_to_os_path(entry.path.as_bytes())?);
        let Ok(metadata) = fs::metadata(&absolute) else {
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
                sley_index::GitlinkStatVerdict::Populated => continue,
                sley_index::GitlinkStatVerdict::TypeChanged => {
                    if !quiet {
                        print_update_index_needs_update(entry.path.as_bytes());
                    }
                    needs_update = true;
                    continue;
                }
            }
        }
        if !metadata.is_file() {
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
        let body = fs::read(&absolute)?;
        let object = EncodedObject::new(ObjectType::Blob, body);
        let oid = object.object_id(format)?;
        if oid != entry.oid || file_mode(&metadata) != entry.mode {
            if !quiet {
                print_update_index_needs_update(entry.path.as_bytes());
            }
            needs_update = true;
            if selected_for_update {
                let updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
                if updated_entry != *entry {
                    *entry = updated_entry;
                    index_dirty = true;
                }
            }
            continue;
        }
        let updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
        if updated_entry != *entry {
            *entry = updated_entry;
            index_dirty = true;
        }
    }
    if index_dirty {
        fs::write(&index_path, index.write(format)?)?;
    }
    if needs_update && !quiet {
        return Err(GitError::Exit(1));
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

fn refresh_all_index_paths_parallel(
    worktree_root: &Path,
    index_path: &Path,
    format: ObjectFormat,
    mut index: Index,
    stat_cache: IndexStatCache,
    quiet: bool,
    ignore_missing: bool,
) -> Result<UpdateIndexResult> {
    let prechecks = tracked_only_non_clean_prechecks_parallel(worktree_root, &index, &stat_cache)?;
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
                let Ok(metadata) = fs::metadata(&absolute) else {
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
                        sley_index::GitlinkStatVerdict::Populated => continue,
                        sley_index::GitlinkStatVerdict::TypeChanged => {
                            if !quiet {
                                print_update_index_needs_update(&path);
                            }
                            needs_update = true;
                            continue;
                        }
                    }
                }
                if !metadata.is_file() {
                    if !quiet {
                        print_update_index_needs_update(&path);
                    }
                    needs_update = true;
                    continue;
                }
                if stat_cache.reuse_index_entry(entry, &metadata).is_some() {
                    continue;
                }
                let body = fs::read(&absolute)?;
                let object = EncodedObject::new(ObjectType::Blob, body);
                let oid = object.object_id(format)?;
                if oid != entry.oid || file_mode(&metadata) != entry.mode {
                    if !quiet {
                        print_update_index_needs_update(&path);
                    }
                    needs_update = true;
                    continue;
                }
                let updated_entry = index_entry_from_metadata(entry.path.clone(), oid, &metadata);
                if updated_entry != *entry {
                    *entry = updated_entry;
                    index_dirty = true;
                }
            }
        }
    }
    if index_dirty {
        fs::write(index_path, index.write(format)?)?;
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
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(UpdateIndexResult {
            entries: 0,
            updated: Vec::new(),
        });
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
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
    if again_paths.is_empty() {
        return Ok(UpdateIndexResult {
            entries: index.entries.len(),
            updated: Vec::new(),
        });
    }
    update_index_paths(worktree_root, git_dir, format, &again_paths, options)
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
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

fn selected_git_paths(worktree_root: &Path, paths: &[PathBuf]) -> Result<BTreeSet<Vec<u8>>> {
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

fn git_path_selected(path: &[u8], selected_paths: &BTreeSet<Vec<u8>>) -> bool {
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
    fs::write(index_path, index.write(format)?)?;
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
    fs::write(index_path, index.write(format)?)?;
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
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated: Vec::new(),
    })
}

fn index_extensions_without_cache_tree(extensions: &[u8]) -> Vec<u8> {
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

pub fn update_index_cacheinfo(
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    entries: &[CacheInfoEntry],
    add: bool,
    verbose: bool,
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
    let mut reports: Vec<String> = Vec::new();
    for cacheinfo in entries {
        if !add
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
        index.entries.retain(|existing| {
            existing.path != cacheinfo.path || index_entry_stage(existing) != cacheinfo.stage
        });
        index.entries.push(entry);
        updated.push(cacheinfo.oid);
        // git's add_cacheinfo() calls report("add '%s'") *after* the entry is
        // staged, regardless of whether the subsequent index write succeeds.
        reports.push(format!(
            "add '{}'",
            String::from_utf8_lossy(&cacheinfo.path)
        ));
    }
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    // git refuses to write an index entry whose object id is the null oid:
    // do_write_index() emits `error: cache entry has null sha1: <path>` and
    // returns nonzero, leaving the on-disk index untouched. The verbose `add`
    // line has already been printed by then.
    let null_entry = index.entries.iter().find(|entry| entry.oid.is_null());
    if let Some(entry) = null_entry {
        if verbose {
            flush_update_index_reports(&reports)?;
        }
        eprintln!(
            "error: cache entry has null sha1: {}",
            String::from_utf8_lossy(&entry.path)
        );
        return Err(GitError::Exit(128));
    }
    fs::write(index_path, index.write(format)?)?;
    if verbose {
        flush_update_index_reports(&reports)?;
    }
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

fn flush_update_index_reports(reports: &[String]) -> Result<()> {
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
    for record in records {
        match record {
            IndexInfoRecord::Remove { path } => {
                index.entries.retain(|existing| existing.path != *path);
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
                updated.push(cacheinfo.oid);
            }
        }
    }
    index.entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| index_entry_stage(left).cmp(&index_entry_stage(right)))
    });
    fs::write(index_path, index.write(format)?)?;
    Ok(UpdateIndexResult {
        entries: index.entries.len(),
        updated,
    })
}

fn index_flags(path_len: usize, stage: u16) -> u16 {
    ((stage & 0x3) << 12) | ((path_len.min(0xfff) as u16) & 0x0fff)
}

const INDEX_FLAG_ASSUME_UNCHANGED: u16 = 0x8000;
const INDEX_FLAG_EXTENDED: u16 = 0x4000;
const INDEX_EXTENDED_FLAG_SKIP_WORKTREE: u16 = 0x4000;

fn normalize_index_version_for_extended_flags(index: &mut Index) {
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

fn index_entry_stage(entry: &IndexEntry) -> u16 {
    (entry.flags >> 12) & 0x3
}

/// The oid of the stage-0 entry in `range` (the path's currently-tracked blob),
/// if any. Used by the safecrlf check to fetch `has_crlf_in_index`.
fn stage0_oid_in_range(entries: &[IndexEntry], range: std::ops::Range<usize>) -> Option<ObjectId> {
    entries[range]
        .iter()
        .find(|entry| index_entry_stage(entry) == 0)
        .map(|entry| entry.oid)
}

fn index_entry_skip_worktree(entry: &IndexEntry) -> bool {
    entry.flags & INDEX_FLAG_EXTENDED != 0
        && entry.flags_extended & INDEX_EXTENDED_FLAG_SKIP_WORKTREE != 0
}

fn print_update_index_path_error(path: &[u8], message: &str) {
    let path = String::from_utf8_lossy(path);
    eprintln!("error: {path}: {message}");
    eprintln!("fatal: Unable to process path {path}");
}

fn print_update_index_needs_update(path: &[u8]) {
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

fn write_tree_from_index_with_options_and_odb(
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
    match BorrowedIndex::parse(&index_bytes, format) {
        Ok(index) => write_tree_from_borrowed_index(&index, format, &options, odb, &mut checker),
        Err(GitError::Unsupported(_)) => {
            let index = Index::parse(&index_bytes, format)?;
            write_tree_from_owned_index(&index, format, &options, odb, &mut checker)
        }
        Err(err) => Err(err),
    }
}

fn write_tree_from_borrowed_index(
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

fn write_tree_from_owned_index(
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
struct WriteTreeEntry<'a> {
    path: &'a [u8],
    mode: u32,
    oid: ObjectId,
}

trait WriteTreeIndexEntry {
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

fn write_tree_entries_for_prefix<'a, E>(
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

fn write_tree_entries_stream<E>(
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

fn valid_cache_tree_oid(tree: Option<&CacheTree>, entry_count: usize) -> Option<ObjectId> {
    let tree = tree?;
    if valid_cache_tree_entry_count(Some(tree))? != entry_count {
        return None;
    }
    tree.oid
}

fn valid_cache_tree_entry_count(tree: Option<&CacheTree>) -> Option<usize> {
    let tree = tree?;
    if tree.entry_count < 0 || tree.oid.is_none() {
        return None;
    }
    Some(tree.entry_count as usize)
}

fn same_tree_component(path: &[u8], prefix: &[u8], name: &[u8]) -> Result<bool> {
    let Some(remainder) = path.strip_prefix(prefix) else {
        return Err(GitError::InvalidPath(format!(
            "invalid index path {}",
            String::from_utf8_lossy(path)
        )));
    };
    Ok(remainder.starts_with(name) && remainder.get(name.len()) == Some(&b'/'))
}

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
struct StatusProfileCounters {
    fast_path_borrowed: bool,
    read_dir_calls: u64,
    dir_entries_seen: u64,
    file_type_calls: u64,
    ignore_checks: u64,
    ignore_pattern_tests: u64,
    ignore_glob_fallback_tests: u64,
    tracked_exact_hits: u64,
    tracked_dir_prefix_hits: u64,
    tracked_skip_worktree_prefix_hits: u64,
    untracked_rows: u64,
    tracked_elapsed_us: u128,
    untracked_elapsed_us: u128,
    render_elapsed_us: u128,
    overlap_enabled: bool,
}

impl StatusProfileCounters {
    fn enabled() -> bool {
        std::env::var_os("SLEY_STATUS_PROFILE").is_some_and(|value| value != "0")
    }

    fn merge_untracked(&mut self, other: StatusProfileCounters) {
        self.read_dir_calls += other.read_dir_calls;
        self.dir_entries_seen += other.dir_entries_seen;
        self.file_type_calls += other.file_type_calls;
        self.ignore_checks += other.ignore_checks;
        self.ignore_pattern_tests += other.ignore_pattern_tests;
        self.ignore_glob_fallback_tests += other.ignore_glob_fallback_tests;
        self.tracked_exact_hits += other.tracked_exact_hits;
        self.tracked_dir_prefix_hits += other.tracked_dir_prefix_hits;
        self.tracked_skip_worktree_prefix_hits += other.tracked_skip_worktree_prefix_hits;
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
            self.untracked_rows,
            self.tracked_elapsed_us,
            self.untracked_elapsed_us,
            self.render_elapsed_us,
            self.overlap_enabled
        );
    }
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

fn collect_short_status_with_options(
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
    let (parsed_index, stat_cache, head_matches_index) =
        read_index_with_stat_cache(git_dir, format, &db)?;
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
            options.untracked_mode,
        );
        let mut entries = entries?;
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
        return Ok(entries);
    }
    let index = index_entries_from_index(parsed_index);
    let head = if head_matches_index {
        None
    } else {
        Some(head_tree_entries(git_dir, format, &db)?)
    };
    let tracked_paths = if options.untracked_mode == StatusUntrackedMode::None {
        Some(index.keys().cloned().collect::<BTreeSet<_>>())
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
            tracked_paths.as_ref(),
            Some(&mut ignores),
        )?;
    let mut entries = Vec::new();
    if head_matches_index {
        collect_status_entries_head_matches_index(
            &index,
            &worktree,
            &tracked_presence,
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
                submodule_dirt_map: &submodule_dirt_map,
                ignores: &ignores,
            },
            options.untracked_mode,
            &mut entries,
        );
    }
    if options.include_ignored {
        let ignored_paths =
            ignored_untracked_paths(worktree_root, git_dir, &index, &ignores, true)?;
        let ignored_paths: Vec<Vec<u8>> = match options.ignored_mode {
            StatusIgnoredMode::Matching => ignored_paths,
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
            .keys()
            .filter(|path| !index.contains_key(*path) && !ignores.is_ignored(path, false))
            .cloned()
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

fn collect_status_entries_head_matches_index(
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    tracked_presence: &HashSet<Vec<u8>>,
    submodule_dirt_map: &BTreeMap<Vec<u8>, u8>,
    untracked_mode: StatusUntrackedMode,
    entries: &mut Vec<ShortStatusEntry>,
) {
    for (path, index_entry) in index {
        let worktree_entry = worktree.get(path);
        let worktree_present =
            worktree_entry.is_some() || tracked_presence.contains(path.as_slice());
        let submodule = status_submodule_from_entries(
            path,
            index_entry,
            worktree_entry,
            submodule_dirt_map,
            untracked_mode,
        );
        let worktree_code = match worktree_entry {
            None if !worktree_present => b'D',
            Some(worktree_entry) if worktree_entry != index_entry => b'M',
            _ if submodule.is_some_and(|sub| sub.any()) => b'M',
            _ => b' ',
        };
        if worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: b' ',
                worktree: worktree_code,
                path: path.clone(),
                head_mode: Some(index_entry.mode),
                index_mode: Some(index_entry.mode),
                worktree_mode: status_worktree_mode(
                    Some(index_entry),
                    worktree_entry,
                    worktree_present,
                ),
                head_oid: Some(index_entry.oid),
                index_oid: Some(index_entry.oid),
                submodule: submodule.filter(|sub| sub.any()),
            });
        }
    }
}

struct StatusComparisonInputs<'a> {
    head: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    index: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    worktree: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    tracked_presence: &'a HashSet<Vec<u8>>,
    submodule_dirt_map: &'a BTreeMap<Vec<u8>, u8>,
    ignores: &'a IgnoreMatcher,
}

fn collect_status_entries_with_head(
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
        let submodule = match index_entry {
            Some(index_entry) => status_submodule_from_entries(
                &path,
                index_entry,
                worktree_entry,
                inputs.submodule_dirt_map,
                untracked_mode,
            ),
            None => None,
        };
        let (index_code, worktree_code) =
            if head_entry.is_none() && index_entry.is_none() && worktree_entry.is_some() {
                (b'?', b'?')
            } else {
                let index_code = match (head_entry, index_entry) {
                    (None, Some(_)) => b'A',
                    (Some(_), None) => b'D',
                    (Some(left), Some(right)) if left != right => b'M',
                    _ => b' ',
                };
                let worktree_code = match (index_entry, worktree_entry) {
                    (None, Some(_)) => b'?',
                    (Some(_), None) if !worktree_present => b'D',
                    (Some(left), Some(right)) if left != right => b'M',
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                (index_code, worktree_code)
            };
        if index_code != b' ' || worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path,
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: index_entry.map(|entry| entry.mode),
                worktree_mode: status_worktree_mode(index_entry, worktree_entry, worktree_present),
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: index_entry.map(|entry| entry.oid),
                submodule: submodule.filter(|sub| sub.any()),
            });
        }
    }
}

fn status_worktree_mode(
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

fn status_submodule_from_entries(
    path: &[u8],
    index_entry: &TrackedEntry,
    worktree_entry: Option<&TrackedEntry>,
    submodule_dirt_map: &BTreeMap<Vec<u8>, u8>,
    untracked_mode: StatusUntrackedMode,
) -> Option<SubmoduleStatus> {
    let worktree_entry = worktree_entry?;
    if !sley_index::is_gitlink(index_entry.mode) || !sley_index::is_gitlink(worktree_entry.mode) {
        return None;
    }
    let dirt = submodule_dirt_map.get(path).copied().unwrap_or(0);
    Some(SubmoduleStatus {
        new_commits: index_entry.oid != worktree_entry.oid,
        modified_content: dirt & DIRTY_SUBMODULE_MODIFIED != 0,
        untracked_content: dirt & DIRTY_SUBMODULE_UNTRACKED != 0
            && !matches!(untracked_mode, StatusUntrackedMode::None),
    })
}

fn short_status_tracked_only(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: &Index,
    stat_cache: &IndexStatCache,
    head_matches_index: bool,
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
            Some(&index_entry)
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
        let index_code = match head_entry {
            None => b'A',
            Some(head_entry) if *head_entry != index_entry => b'M',
            _ => b' ',
        };
        let worktree_code = match worktree_entry.as_ref() {
            None => b'D',
            Some(worktree_entry) if *worktree_entry != index_entry => b'M',
            _ if submodule.is_some_and(|sub| sub.any()) => b'M',
            _ => b' ',
        };
        if index_code != b' ' || worktree_code != b' ' {
            entries.push(ShortStatusEntry {
                index: index_code,
                worktree: worktree_code,
                path: path.to_vec(),
                head_mode: head_entry.map(|entry| entry.mode),
                index_mode: Some(index_entry.mode),
                worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: Some(index_entry.oid),
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

fn short_status_borrowed_head_matches_index_if_possible(
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
    let index_bytes = fs::read(&index_path)?;
    let borrowed = match BorrowedIndex::parse(&index_bytes, format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    if !head_matches_borrowed_index_from_cache_tree(
        &borrowed,
        format,
        &head_tree_oid,
        stage0_entry_count,
    )? {
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
        let entries = short_status_borrowed_tracked_only_head_matches_index_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(entries));
    }

    if stage0_entry_count < 8192 {
        let tracked_start = Instant::now();
        let mut entries = short_status_borrowed_tracked_only_head_matches_index_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
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
    if profile_enabled {
        let (mut entries, untracked_paths, untracked_profile) =
            std::thread::scope(|scope| -> Result<_> {
                let tracked = scope.spawn(|| {
                    let start = Instant::now();
                    short_status_borrowed_tracked_only_head_matches_index_parallel(
                        worktree_root,
                        git_dir,
                        format,
                        &borrowed,
                        &stat_cache,
                        untracked_mode,
                    )
                    .map(|entries| (entries, start.elapsed().as_micros()))
                });
                let untracked = scope.spawn(|| -> Result<(Vec<Vec<u8>>, StatusProfileCounters)> {
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
                });
                let (entries, tracked_elapsed_us) = tracked
                    .join()
                    .map_err(|_| GitError::Command("status worker panicked".into()))??;
                let (untracked_paths, untracked_profile) = untracked
                    .join()
                    .map_err(|_| GitError::Command("status worker panicked".into()))??;
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
        let tracked = scope.spawn(|| {
            short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
                untracked_mode,
            )
        });
        let untracked = scope.spawn(|| -> Result<Vec<Vec<u8>>> {
            let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
            status_untracked_paths_from_borrowed_index(
                worktree_root,
                git_dir,
                &borrowed,
                &mut ignores,
                untracked_mode,
                None,
            )
        });
        let entries = tracked
            .join()
            .map_err(|_| GitError::Command("status worker panicked".into()))??;
        let untracked_paths = untracked
            .join()
            .map_err(|_| GitError::Command("status worker panicked".into()))??;
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

fn stream_short_status_borrowed_head_matches_index_if_possible<F>(
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
    let index_bytes = fs::read(&index_path)?;
    let borrowed = match BorrowedIndex::parse(&index_bytes, format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    if !head_matches_borrowed_index_from_cache_tree(
        &borrowed,
        format,
        &head_tree_oid,
        stage0_entry_count,
    )? {
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
        let tracked_control =
            stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
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

    if stage0_entry_count < 8192 {
        let tracked_start = Instant::now();
        let tracked_control =
            stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                worktree_root,
                git_dir,
                format,
                &borrowed,
                &stat_cache,
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
    let (tracked_control, untracked_paths, untracked_profile) =
        std::thread::scope(|scope| -> Result<_> {
            let untracked = scope.spawn(|| -> Result<(Vec<Vec<u8>>, StatusProfileCounters)> {
                let mut local_profile = StatusProfileCounters::default();
                let mut ignores = IgnoreMatcher::from_worktree_base(worktree_root)?;
                let start = Instant::now();
                let paths = status_untracked_paths_from_borrowed_index(
                    worktree_root,
                    git_dir,
                    &borrowed,
                    &mut ignores,
                    untracked_mode,
                    profile_enabled.then_some(&mut local_profile),
                )?;
                local_profile.untracked_elapsed_us = start.elapsed().as_micros();
                local_profile.untracked_rows = paths.len() as u64;
                Ok((paths, local_profile))
            });
            let tracked_start = Instant::now();
            let tracked_control =
                stream_short_status_borrowed_tracked_only_head_matches_index_parallel(
                    worktree_root,
                    git_dir,
                    format,
                    &borrowed,
                    &stat_cache,
                    untracked_mode,
                    emit,
                )?;
            let tracked_elapsed_us = tracked_start.elapsed().as_micros();
            let (untracked_paths, untracked_profile) = untracked
                .join()
                .map_err(|_| GitError::Command("status worker panicked".into()))??;
            if let Some(profile) = profile.as_mut() {
                profile.tracked_elapsed_us = tracked_elapsed_us;
            }
            Ok((
                tracked_control,
                untracked_paths,
                profile_enabled.then_some(untracked_profile),
            ))
        })?;
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
    Ok(Some(()))
}

fn short_status_borrowed_head_matches_index_count_if_possible(
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
    let index_bytes = fs::read(&index_path)?;
    let borrowed = match BorrowedIndex::parse(&index_bytes, format) {
        Ok(index) => index,
        Err(GitError::Unsupported(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let Some(head_tree_oid) = resolve_head_tree_oid(git_dir, format, db)? else {
        return Ok(None);
    };
    let stage0_entry_count = borrowed
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
        .count();
    if !head_matches_borrowed_index_from_cache_tree(
        &borrowed,
        format,
        &head_tree_oid,
        stage0_entry_count,
    )? {
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
            untracked_mode,
        )?;
        if let Some(profile) = profile.as_mut() {
            profile.tracked_elapsed_us = tracked_start.elapsed().as_micros();
            profile.emit();
        }
        return Ok(Some(count));
    }

    if stage0_entry_count < 8192 {
        let tracked_start = Instant::now();
        let tracked_count = short_status_borrowed_tracked_only_head_matches_index_count_parallel(
            worktree_root,
            git_dir,
            format,
            &borrowed,
            &stat_cache,
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
    let (tracked_count, untracked_count, untracked_profile) =
        std::thread::scope(|scope| -> Result<_> {
            let tracked = scope.spawn(|| {
                let start = Instant::now();
                short_status_borrowed_tracked_only_head_matches_index_count_parallel(
                    worktree_root,
                    git_dir,
                    format,
                    &borrowed,
                    &stat_cache,
                    untracked_mode,
                )
                .map(|count| (count, start.elapsed().as_micros()))
            });
            let untracked = scope.spawn(|| -> Result<(usize, StatusProfileCounters)> {
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
            });
            let (tracked_count, tracked_elapsed_us) = tracked
                .join()
                .map_err(|_| GitError::Command("status worker panicked".into()))??;
            let (untracked_count, untracked_profile) = untracked
                .join()
                .map_err(|_| GitError::Command("status worker panicked".into()))??;
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

fn emit_untracked_status_entry<'a, F>(
    emit: &'a mut F,
) -> impl FnMut(&[u8]) -> Result<StreamControl> + 'a
where
    F: for<'row> FnMut(ShortStatusRow<'row>) -> Result<StreamControl>,
{
    |path| emit(untracked_status_row(path))
}

fn untracked_status_entry(path: Vec<u8>) -> ShortStatusEntry {
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

fn untracked_status_row(path: &[u8]) -> ShortStatusRow<'_> {
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

fn append_untracked_status_entries(
    entries: &mut Vec<ShortStatusEntry>,
    untracked_paths: Vec<Vec<u8>>,
) {
    for path in untracked_paths {
        entries.push(untracked_status_entry(path));
    }
}

#[derive(Debug, Clone, Copy)]
enum TrackedOnlyPrecheck {
    Deleted(usize),
    Slow(usize),
}

#[derive(Debug)]
enum TrackedOnlyPrecheckOutcome {
    Clean,
    Deleted,
    Slow,
}

fn short_status_tracked_only_head_matches_index_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    stat_cache: &IndexStatCache,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks = tracked_only_non_clean_prechecks_parallel(worktree_root, index, stat_cache)?;

    let mut clean_filter = None;
    let mut entries = Vec::new();
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
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
                    None => b'D',
                    Some(worktree_entry) if *worktree_entry != index_entry => b'M',
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    entries.push(ShortStatusEntry {
                        index: b' ',
                        worktree: worktree_code,
                        path: path.to_vec(),
                        head_mode: Some(index_entry.mode),
                        index_mode: Some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: Some(index_entry.oid),
                        index_oid: Some(index_entry.oid),
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

fn short_status_borrowed_tracked_only_head_matches_index_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks =
        tracked_only_borrowed_non_clean_prechecks_parallel(worktree_root, index, stat_cache)?;

    let mut clean_filter = None;
    let mut entries = Vec::new();
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
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
                    None => b'D',
                    Some(worktree_entry) if *worktree_entry != index_entry => b'M',
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    entries.push(ShortStatusEntry {
                        index: b' ',
                        worktree: worktree_code,
                        path: entry.path.to_vec(),
                        head_mode: Some(index_entry.mode),
                        index_mode: Some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: Some(index_entry.oid),
                        index_oid: Some(index_entry.oid),
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

fn stream_short_status_borrowed_tracked_only_head_matches_index_parallel<F>(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    untracked_mode: StatusUntrackedMode,
    emit: &mut F,
) -> Result<StreamControl>
where
    F: for<'a> FnMut(ShortStatusRow<'a>) -> Result<StreamControl>,
{
    let prechecks =
        tracked_only_borrowed_non_clean_prechecks_parallel(worktree_root, index, stat_cache)?;

    let mut clean_filter = None;
    for precheck in prechecks {
        match precheck {
            TrackedOnlyPrecheck::Deleted(idx) => {
                let entry = &index.entries[idx];
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
                    None => b'D',
                    Some(worktree_entry) if *worktree_entry != index_entry => b'M',
                    _ if submodule.is_some_and(|sub| sub.any()) => b'M',
                    _ => b' ',
                };
                if worktree_code != b' ' {
                    if emit(ShortStatusRow {
                        index: b' ',
                        worktree: worktree_code,
                        path: entry.path,
                        head_mode: Some(index_entry.mode),
                        index_mode: Some(index_entry.mode),
                        worktree_mode: worktree_entry.as_ref().map(|entry| entry.mode),
                        head_oid: Some(index_entry.oid),
                        index_oid: Some(index_entry.oid),
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

fn short_status_borrowed_tracked_only_head_matches_index_count_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
    untracked_mode: StatusUntrackedMode,
) -> Result<usize> {
    let prechecks =
        tracked_only_borrowed_non_clean_prechecks_parallel(worktree_root, index, stat_cache)?;

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
                    Some(worktree_entry) if *worktree_entry != index_entry => b'M',
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

fn short_status_tracked_only_with_head_parallel(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index: &Index,
    stat_cache: &IndexStatCache,
    head: &BTreeMap<Vec<u8>, TrackedEntry>,
    untracked_mode: StatusUntrackedMode,
) -> Result<Vec<ShortStatusEntry>> {
    let prechecks = tracked_only_non_clean_prechecks_parallel(worktree_root, index, stat_cache)?;
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
        let index_code = match head_entry {
            None => b'A',
            Some(head_entry) if *head_entry != index_entry => b'M',
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
            None => (b' ', Some(index_entry.mode), None),
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
                    None => b'D',
                    Some(worktree_entry) if *worktree_entry != index_entry => b'M',
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
                index_mode: Some(index_entry.mode),
                worktree_mode,
                head_oid: head_entry.map(|entry| entry.oid),
                index_oid: Some(index_entry.oid),
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

fn tracked_only_precheck_index(precheck: TrackedOnlyPrecheck) -> usize {
    match precheck {
        TrackedOnlyPrecheck::Deleted(idx) | TrackedOnlyPrecheck::Slow(idx) => idx,
    }
}

fn tracked_only_non_clean_prechecks_parallel(
    worktree_root: &Path,
    index: &Index,
    stat_cache: &IndexStatCache,
) -> Result<Vec<TrackedOnlyPrecheck>> {
    let normal_indices = index
        .entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| (entry.stage() == Stage::Normal).then_some(idx))
        .collect::<Vec<_>>();
    if normal_indices.is_empty() {
        return Ok(Vec::new());
    }
    let max_workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(16);
    let worker_count = max_workers.min(normal_indices.len().div_ceil(512)).max(1);
    if worker_count == 1 {
        let mut prechecks = Vec::new();
        let mut absolute = PathBuf::new();
        for idx in normal_indices {
            let entry = &index.entries[idx];
            match tracked_only_stat_precheck(worktree_root, entry, stat_cache, &mut absolute)? {
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
    let chunk_size = normal_indices.len().div_ceil(worker_count);
    let mut prechecks = std::thread::scope(|scope| -> Result<Vec<TrackedOnlyPrecheck>> {
        let mut handles = Vec::new();
        for chunk in normal_indices.chunks(chunk_size) {
            handles.push(scope.spawn(move || -> Result<Vec<TrackedOnlyPrecheck>> {
                let mut prechecks = Vec::new();
                let mut absolute = PathBuf::new();
                for &idx in chunk {
                    let entry = &index.entries[idx];
                    match tracked_only_stat_precheck(
                        worktree_root,
                        entry,
                        stat_cache,
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
                Ok(prechecks)
            }));
        }
        let mut prechecks = Vec::new();
        for handle in handles {
            let mut chunk = handle
                .join()
                .map_err(|_| GitError::Command("status worker panicked".into()))??;
            prechecks.append(&mut chunk);
        }
        Ok(prechecks)
    })?;
    prechecks.sort_by_key(|precheck| match precheck {
        TrackedOnlyPrecheck::Deleted(idx) | TrackedOnlyPrecheck::Slow(idx) => *idx,
    });
    Ok(prechecks)
}

fn tracked_only_borrowed_non_clean_prechecks_parallel(
    worktree_root: &Path,
    index: &BorrowedIndex<'_>,
    stat_cache: &IndexStatCache,
) -> Result<Vec<TrackedOnlyPrecheck>> {
    let normal_indices = index
        .entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| (entry.stage() == Stage::Normal).then_some(idx))
        .collect::<Vec<_>>();
    if normal_indices.is_empty() {
        return Ok(Vec::new());
    }
    let max_workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(16);
    let worker_count = max_workers.min(normal_indices.len().div_ceil(512)).max(1);
    if worker_count == 1 {
        let mut prechecks = Vec::new();
        let mut absolute = PathBuf::new();
        for idx in normal_indices {
            let entry = &index.entries[idx];
            match tracked_only_borrowed_stat_precheck(
                worktree_root,
                entry,
                stat_cache,
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
    let chunk_size = normal_indices.len().div_ceil(worker_count);
    let mut prechecks = std::thread::scope(|scope| -> Result<Vec<TrackedOnlyPrecheck>> {
        let mut handles = Vec::new();
        for chunk in normal_indices.chunks(chunk_size) {
            handles.push(scope.spawn(move || -> Result<Vec<TrackedOnlyPrecheck>> {
                let mut prechecks = Vec::new();
                let mut absolute = PathBuf::new();
                for &idx in chunk {
                    let entry = &index.entries[idx];
                    match tracked_only_borrowed_stat_precheck(
                        worktree_root,
                        entry,
                        stat_cache,
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
                Ok(prechecks)
            }));
        }
        let mut prechecks = Vec::new();
        for handle in handles {
            let mut chunk = handle
                .join()
                .map_err(|_| GitError::Command("status worker panicked".into()))??;
            prechecks.append(&mut chunk);
        }
        Ok(prechecks)
    })?;
    prechecks.sort_by_key(|precheck| match precheck {
        TrackedOnlyPrecheck::Deleted(idx) | TrackedOnlyPrecheck::Slow(idx) => *idx,
    });
    Ok(prechecks)
}

fn tracked_only_stat_precheck(
    worktree_root: &Path,
    index_entry: &IndexEntry,
    stat_cache: &IndexStatCache,
    absolute: &mut PathBuf,
) -> Result<TrackedOnlyPrecheckOutcome> {
    if sley_index::is_gitlink(index_entry.mode) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
    }
    let git_path = index_entry.path.as_bytes();
    set_worktree_path_from_repo_path(worktree_root, git_path, absolute)?;
    let metadata = match fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
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

fn tracked_only_borrowed_stat_precheck(
    worktree_root: &Path,
    index_entry: &IndexEntryRef<'_>,
    stat_cache: &IndexStatCache,
    absolute: &mut PathBuf,
) -> Result<TrackedOnlyPrecheckOutcome> {
    if sley_index::is_gitlink(index_entry.mode) {
        return Ok(TrackedOnlyPrecheckOutcome::Slow);
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

fn set_worktree_path_from_repo_path(
    worktree_root: &Path,
    git_path: &[u8],
    out: &mut PathBuf,
) -> Result<()> {
    out.clear();
    out.push(worktree_root);
    push_repo_path(out, git_path)
}

#[cfg(unix)]
fn push_repo_path(out: &mut PathBuf, path: &[u8]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    out.push(Path::new(std::ffi::OsStr::from_bytes(path)));
    Ok(())
}

#[cfg(not(unix))]
fn push_repo_path(out: &mut PathBuf, path: &[u8]) -> Result<()> {
    let path = std::str::from_utf8(path)
        .map_err(|_| GitError::InvalidPath("index path is not utf8".into()))?;
    for component in path.split('/') {
        out.push(component);
    }
    Ok(())
}

fn tracked_only_submodule_status(
    worktree_root: &Path,
    path: &[u8],
    index_entry: &TrackedEntry,
    worktree_entry: Option<&TrackedEntry>,
    untracked_mode: StatusUntrackedMode,
) -> Result<Option<SubmoduleStatus>> {
    let Some(worktree_entry) = worktree_entry else {
        return Ok(None);
    };
    if !sley_index::is_gitlink(index_entry.mode) || !sley_index::is_gitlink(worktree_entry.mode) {
        return Ok(None);
    }
    let absolute = worktree_root.join(repo_path_to_os_path(path)?);
    let dirt = if absolute.is_dir() {
        submodule_dirt(&absolute)
    } else {
        0
    };
    Ok(Some(SubmoduleStatus {
        new_commits: index_entry.oid != worktree_entry.oid,
        modified_content: dirt & DIRTY_SUBMODULE_MODIFIED != 0,
        untracked_content: dirt & DIRTY_SUBMODULE_UNTRACKED != 0
            && !matches!(untracked_mode, StatusUntrackedMode::None),
    }))
}

fn status_sort_category(entry: &ShortStatusEntry) -> u8 {
    match (entry.index, entry.worktree) {
        (b'?', b'?') => 1,
        (b'!', b'!') => 2,
        _ => 0,
    }
}

pub fn untracked_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<Vec<u8>>> {
    untracked_paths_with_options(
        worktree_root,
        git_dir,
        format,
        UntrackedPathOptions::default(),
    )
}

/// Pathspec filter for untracked collection. Mirrors git `ls-files` pathspec
/// semantics: literal paths, recursive directory prefixes, and fnmatch globs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedPathspecFilter {
    pub path: Vec<u8>,
    pub recursive: bool,
    pub is_glob: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UntrackedPathOptions {
    pub directory: bool,
    pub no_empty_directory: bool,
    pub preserve_ignored_directories: bool,
    pub exclude_standard: bool,
    pub ignored_only: bool,
    pub exclude_patterns: Vec<Vec<u8>>,
    pub exclude_per_directory: Vec<String>,
    pub pathspecs: Vec<UntrackedPathspecFilter>,
}

// The wildmatch engine and the single-item pathspec matcher now live in the
// shared `sley-pathspec` crate. Re-export them so existing `sley-worktree`
// callers (and the t3070 `ls-files` path) keep their public surface unchanged.
pub use sley_pathspec::{
    PathspecMatchMagic, WM_CASEFOLD, WM_PATHNAME, pathspec_is_glob, pathspec_item_matches,
    wildmatch,
};

/// Whether `path` matches an `ls-files` pathspec (literal, directory prefix, or glob).
pub fn untracked_pathspec_matches(spec: &UntrackedPathspecFilter, path: &[u8]) -> bool {
    if spec.path.is_empty() {
        return true;
    }
    let path_no_slash = path.strip_suffix(b"/").unwrap_or(path);
    if path == spec.path.as_slice() || path_no_slash == spec.path.as_slice() {
        return true;
    }
    if spec.recursive
        && let Some(rest) = path
            .strip_prefix(spec.path.as_slice())
            .and_then(|rest| rest.strip_prefix(b"/"))
        && !rest.is_empty()
    {
        return true;
    }
    if spec.is_glob {
        return untracked_wildmatch(&spec.path, path)
            || untracked_wildmatch(&spec.path, path_no_slash);
    }
    false
}

/// Whether a directory walk must descend into `parent` to satisfy active pathspecs.
pub fn untracked_pathspec_needs_descent(parent: &[u8], specs: &[UntrackedPathspecFilter]) -> bool {
    if specs.is_empty() {
        return false;
    }
    let parent_prefix = if parent.is_empty() {
        Vec::new()
    } else {
        let mut prefix = parent.to_vec();
        prefix.push(b'/');
        prefix
    };
    for spec in specs {
        if !parent.is_empty()
            && spec.path.starts_with(&parent_prefix)
            && spec.path.as_slice() != parent
        {
            return true;
        }
        if spec.is_glob && glob_pathspec_may_match_under(&spec.path, parent) {
            return true;
        }
        if spec.recursive
            && !parent.is_empty()
            && parent.starts_with(spec.path.as_slice())
            && parent != spec.path.as_slice()
        {
            return true;
        }
    }
    false
}

/// Whether some pathspec selects the directory `git_path` *as a whole* (so an
/// untracked directory can roll up to `dir/` under `--directory`), as opposed to
/// only matching something strictly below it (which forces descent). A
/// directory-prefix pathspec covering the directory, an exact directory match, or
/// a glob matching the directory's own name all count; a deeper glob such as
/// `dir/*.c` or an exact file path inside the directory does not.
fn untracked_pathspec_selects_directory(
    specs: &[UntrackedPathspecFilter],
    git_path: &[u8],
) -> bool {
    specs
        .iter()
        .any(|spec| untracked_pathspec_matches(spec, git_path))
}

fn glob_pathspec_may_match_under(pattern: &[u8], dir: &[u8]) -> bool {
    let literal_prefix = literal_prefix_before_glob(pattern);
    if literal_prefix.is_empty() {
        return true;
    }
    if dir.is_empty() {
        return true;
    }
    let mut dir_prefix = dir.to_vec();
    dir_prefix.push(b'/');
    if literal_prefix.starts_with(&dir_prefix) {
        return true;
    }
    if dir_prefix.starts_with(&literal_prefix) {
        return true;
    }
    literal_prefix
        .strip_suffix(b"/")
        .is_some_and(|prefix| prefix == dir)
}

fn literal_prefix_before_glob(pattern: &[u8]) -> Vec<u8> {
    let mut prefix = Vec::new();
    for &byte in pattern {
        if matches!(byte, b'*' | b'?' | b'[') {
            break;
        }
        prefix.push(byte);
    }
    prefix
}

fn insert_untracked_directory(paths: &mut BTreeSet<Vec<u8>>, git_path: &[u8]) {
    let mut directory = git_path.to_vec();
    if directory.last() != Some(&b'/') {
        directory.push(b'/');
    }
    paths.insert(directory);
}

/// fnmatch-style glob where `*` and `?` match any byte including `/`.
fn untracked_wildmatch(pattern: &[u8], text: &[u8]) -> bool {
    // Untracked-walk pathspec globs match with PATHMATCH semantics (`*` crosses
    // `/`), matching git's default (non-GLOB-magic) pathspec behavior.
    wildmatch(pattern, text, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreMatch {
    pub source: Vec<u8>,
    pub line_number: usize,
    pub pattern: Vec<u8>,
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeState {
    Set,
    Unset,
    Value(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeCheck {
    pub attribute: Vec<u8>,
    pub state: Option<AttributeState>,
}

pub fn untracked_paths_with_options(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    options: UntrackedPathOptions,
) -> Result<Vec<Vec<u8>>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let (index, stat_cache, _) = read_index_entries_with_stat_cache(git_dir, format, &db)?;
    let ignores = IgnoreMatcher::from_sources(
        worktree_root,
        options.exclude_standard,
        &options.exclude_patterns,
        &options.exclude_per_directory,
    )?;
    if options.ignored_only {
        return ignored_untracked_paths(
            worktree_root,
            git_dir,
            &index,
            &ignores,
            options.directory,
        );
    }
    if options.directory {
        let mut paths = BTreeSet::new();
        collect_untracked_directory_paths(
            worktree_root,
            git_dir,
            worktree_root,
            &index,
            &ignores,
            &options,
            &mut paths,
        )?;
        return Ok(paths.into_iter().collect());
    }
    let worktree = worktree_entries_with_stat_cache(
        worktree_root,
        git_dir,
        format,
        Some(&stat_cache),
        None,
        None,
    )?;
    Ok(ls_files_untracked_paths_from_worktree(
        &worktree, &index, &ignores,
    ))
}

/// Untracked paths for `ls-files --others` (without `--directory`): every
/// untracked file is listed individually, except embedded-repository boundaries
/// which are emitted as `dir/` to match git's non-submodule `.git` handling.
fn ls_files_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path) || ignores.is_ignored(path, false) {
            continue;
        }
        if entry.mode == 0o040000 && entry.oid.is_null() {
            insert_untracked_directory(&mut paths, path);
            continue;
        }
        paths.insert(path.clone());
    }
    paths.into_iter().collect()
}

pub fn path_matches_standard_ignore(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
) -> Result<bool> {
    path_matches_ignore(worktree_root, path, is_dir, true, &[])
}

pub fn standard_ignore_match(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
) -> Result<Option<IgnoreMatch>> {
    let ignores = IgnoreMatcher::from_worktree_root(worktree_root.as_ref())?;
    Ok(ignores.match_for(path, is_dir).map(IgnorePattern::to_match))
}

pub fn standard_attributes_for_path(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let matcher = AttributeMatcher::from_worktree_root(worktree_root.as_ref())?;
    Ok(matcher.attributes_for_path(path, requested, all))
}

/// A reusable matcher for standard worktree attributes (global or
/// `core.attributesFile`, every in-tree `.gitattributes`, and
/// `$GIT_DIR/info/attributes`).
///
/// This is behaviourally identical to [`standard_attributes_for_path`] except
/// the attribute sources are read once and reused for each path.
pub struct StandardAttributeMatcher {
    matcher: AttributeMatcher,
}

impl StandardAttributeMatcher {
    pub fn from_worktree_root(worktree_root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            matcher: AttributeMatcher::from_worktree_root(worktree_root.as_ref())?,
        })
    }

    pub fn attributes_for_path(
        &self,
        path: &[u8],
        requested: &[Vec<u8>],
        all: bool,
    ) -> Vec<AttributeCheck> {
        self.matcher.attributes_for_path(path, requested, all)
    }
}

pub fn standard_attributes_for_path_from_tree(
    worktree_root: impl AsRef<Path>,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let mut matcher = AttributeMatcher::default();
    let worktree_root = worktree_root.as_ref();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher.attributes_for_path(path, requested, all))
}

pub fn standard_attributes_for_path_from_index(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    path: &[u8],
    requested: &[Vec<u8>],
    all: bool,
) -> Result<Vec<AttributeCheck>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let mut matcher = AttributeMatcher::default();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    collect_attribute_patterns_from_index(git_dir, format, &db, &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher.attributes_for_path(path, requested, all))
}

pub fn path_matches_ignore(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
) -> Result<bool> {
    path_matches_ignore_with_per_directory(
        worktree_root,
        path,
        is_dir,
        exclude_standard,
        exclude_patterns,
        &[],
    )
}

pub fn path_matches_ignore_with_per_directory(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    is_dir: bool,
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
    exclude_per_directory: &[String],
) -> Result<bool> {
    let ignores = IgnoreMatcher::from_sources(
        worktree_root.as_ref(),
        exclude_standard,
        exclude_patterns,
        exclude_per_directory,
    )?;
    Ok(ignores.is_ignored(path, is_dir))
}

pub fn ignored_index_entries<'a>(
    worktree_root: impl AsRef<Path>,
    entries: &'a [IndexEntry],
    exclude_standard: bool,
    exclude_patterns: &[Vec<u8>],
    exclude_per_directory: &[String],
) -> Result<Vec<&'a IndexEntry>> {
    let ignores = IgnoreMatcher::from_sources(
        worktree_root.as_ref(),
        exclude_standard,
        exclude_patterns,
        exclude_per_directory,
    )?;
    Ok(entries
        .iter()
        .filter(|entry| ignores.is_ignored(entry.path.as_bytes(), false))
        .collect())
}

fn collect_untracked_directory_paths(
    root: &Path,
    git_dir: &Path,
    dir: &Path,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
    options: &UntrackedPathOptions,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_embedded_git_internals(root, &path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            continue;
        }
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path) {
                insert_untracked_directory(paths, &git_path);
                continue;
            }
            let has_tracked_below = index_has_path_under(index, &git_path);
            let needs_descent = untracked_pathspec_needs_descent(&git_path, &options.pathspecs);
            if has_tracked_below {
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if needs_descent {
                // A pathspec reaches into this wholly-untracked directory. Git's
                // `--directory` still rolls it up to `dir/` when a pathspec selects
                // the directory *as a whole* (a directory-prefix that covers it, or
                // a glob matching its name). It descends only when a pathspec
                // targets something strictly below it that does not select the
                // directory itself (e.g. a deeper glob like `dir/*.c` or an exact
                // file path).
                if untracked_pathspec_selects_directory(&options.pathspecs, &git_path) {
                    insert_untracked_directory(paths, &git_path);
                    continue;
                }
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if options.preserve_ignored_directories
                && directory_has_ignored(&path, root, git_dir, ignores)?
            {
                collect_untracked_directory_paths(
                    root, git_dir, &path, index, ignores, options, paths,
                )?;
            } else if !options.no_empty_directory
                || directory_has_file(&path, root, git_dir, ignores)?
            {
                insert_untracked_directory(paths, &git_path);
            }
        } else if !index.contains_key(&git_path)
            && (metadata.is_file() || metadata.file_type().is_symlink())
            && (options.pathspecs.is_empty()
                || options
                    .pathspecs
                    .iter()
                    .any(|spec| untracked_pathspec_matches(spec, &git_path)))
        {
            // A file reached here was found by descending into its parent
            // directory, which happens only when that directory is not eligible
            // for rollup (it contains tracked content, has ignored entries `-d`
            // must preserve, or a pathspec selects something strictly below it).
            // Git's `--directory` rollup is a directory-level decision made when
            // the whole directory matches; an individually-reached file is always
            // listed individually.
            paths.insert(git_path);
        }
    }
    Ok(())
}

fn index_has_path_under(index: &BTreeMap<Vec<u8>, TrackedEntry>, directory: &[u8]) -> bool {
    // The index map is sorted, so a single range query finds whether any tracked
    // path lives under `directory/` in O(log n) — scanning every key was O(n) per
    // untracked directory (quadratic over a deep untracked tree).
    let mut prefix = directory.to_vec();
    prefix.push(b'/');
    index
        .range::<[u8], _>((
            std::ops::Bound::Included(prefix.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .is_some_and(|(path, _)| path.starts_with(&prefix))
}

/// Derives normal-mode untracked paths (directory rollup) from the worktree map
/// produced by the single status walk, avoiding a third filesystem traversal.
fn normal_untracked_paths_from_worktree(
    worktree: &BTreeMap<Vec<u8>, TrackedEntry>,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<Vec<u8>> {
    let mut paths = BTreeSet::new();
    for (path, entry) in worktree {
        if index.contains_key(path) || ignores.is_ignored(path, false) {
            continue;
        }
        if entry.mode == 0o040000 && entry.oid.is_null() {
            insert_untracked_directory(&mut paths, path);
            continue;
        }
        paths.insert(untracked_normal_rollup_path(path, index, ignores));
    }
    paths.into_iter().collect()
}

fn status_untracked_paths_from_index(
    root: &Path,
    git_dir: &Path,
    index: &Index,
    stat_cache: &IndexStatCache,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<Vec<u8>>> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let tracked_dirs = stage0_tracked_directories(index);
    let tracked = IndexStatusLookup {
        stat_cache,
        tracked_dirs: &tracked_dirs,
    };
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    collect_status_untracked_paths(&mut context, root, &[], &mut paths)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn status_untracked_paths_from_borrowed_index(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<Vec<u8>>> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    let tracked = BorrowedIndexLookup::new(&index.entries);
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    collect_status_untracked_paths(&mut context, root, &[], &mut paths)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn stream_status_untracked_paths_from_borrowed_index<F>(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
    mut emit: F,
) -> Result<()>
where
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(());
    }
    let tracked = BorrowedIndexLookup::new(&index.entries);
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    stream_status_untracked_paths(&mut context, root, &[], &mut emit).map(|_| ())
}

fn status_untracked_count_from_borrowed_index(
    root: &Path,
    git_dir: &Path,
    index: &BorrowedIndex<'_>,
    ignores: &mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&mut StatusProfileCounters>,
) -> Result<usize> {
    if matches!(untracked_mode, StatusUntrackedMode::None) {
        return Ok(0);
    }
    let tracked = BorrowedIndexLookup::new(&index.entries);
    let mut context = StatusUntrackedWalk {
        git_dir,
        tracked: &tracked,
        ignores,
        untracked_mode,
        profile,
    };
    let mut count = 0usize;
    count_status_untracked_paths(&mut context, root, &[], &mut count)?;
    Ok(count)
}

trait StatusTrackedLookup {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind>;
    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTrackedKind {
    File,
    Gitlink,
    SkipWorktree,
}

impl StatusTrackedKind {
    fn from_mode_and_skip(mode: u32, skip_worktree: bool) -> Self {
        if sley_index::is_gitlink(mode) {
            Self::Gitlink
        } else if skip_worktree {
            Self::SkipWorktree
        } else {
            Self::File
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTrackedDirectoryKind {
    ContainsTracked,
    TrackedExcluded,
}

struct IndexStatusLookup<'a> {
    stat_cache: &'a IndexStatCache,
    tracked_dirs: &'a HashSet<&'a [u8]>,
}

impl StatusTrackedLookup for IndexStatusLookup<'_> {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind> {
        self.stat_cache.entries.get(git_path).map(|entry| {
            StatusTrackedKind::from_mode_and_skip(entry.mode, entry.is_skip_worktree())
        })
    }

    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind> {
        self.tracked_dirs
            .contains(git_path)
            .then_some(StatusTrackedDirectoryKind::ContainsTracked)
    }
}

struct BorrowedIndexLookup<'a> {
    entries: &'a [IndexEntryRef<'a>],
    tracked: HashMap<&'a [u8], StatusTrackedKind>,
}

impl<'a> BorrowedIndexLookup<'a> {
    fn new(entries: &'a [IndexEntryRef<'a>]) -> Self {
        let mut tracked = HashMap::with_capacity(entries.len());
        for entry in entries {
            if entry.stage() != Stage::Normal {
                continue;
            }
            let path = entry.path;
            tracked.insert(
                path,
                StatusTrackedKind::from_mode_and_skip(entry.mode, entry.is_skip_worktree()),
            );
        }
        Self { entries, tracked }
    }
}

impl StatusTrackedLookup for BorrowedIndexLookup<'_> {
    fn tracked_kind(&self, git_path: &[u8]) -> Option<StatusTrackedKind> {
        self.tracked.get(git_path).copied()
    }

    fn tracked_directory_kind(&self, git_path: &[u8]) -> Option<StatusTrackedDirectoryKind> {
        let mut prefix = git_path.to_vec();
        prefix.push(b'/');
        let start = self
            .entries
            .partition_point(|entry| entry.path < prefix.as_slice());
        let mut saw_normal = false;
        for entry in self.entries[start..]
            .iter()
            .take_while(|entry| entry.path.starts_with(&prefix))
        {
            if entry.stage() != Stage::Normal {
                continue;
            }
            saw_normal = true;
            if !entry.is_skip_worktree() {
                return Some(StatusTrackedDirectoryKind::ContainsTracked);
            }
        }
        saw_normal.then_some(StatusTrackedDirectoryKind::TrackedExcluded)
    }
}

struct StatusUntrackedWalk<'a, T: StatusTrackedLookup + ?Sized> {
    git_dir: &'a Path,
    tracked: &'a T,
    ignores: &'a mut IgnoreMatcher,
    untracked_mode: StatusUntrackedMode,
    profile: Option<&'a mut StatusProfileCounters>,
}

fn collect_status_untracked_paths<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    paths: &mut Vec<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let ignore_len = context.ignores.patterns.len();
    let entries = read_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    let result = (|| -> Result<()> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<()> {
                if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_exact_hits += 1;
                    }
                    if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                        || tracked_kind == StatusTrackedKind::Gitlink
                    {
                        return Ok(());
                    }
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.file_type_calls += 1;
                    }
                    let file_type = entry.file_type()?;
                    if file_type.is_dir() {
                        let path = entry.path();
                        if !is_same_path(&path, context.git_dir) {
                            collect_status_untracked_paths(context, &path, &git_path, paths)?;
                        }
                    }
                    return Ok(());
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type()?;
                let is_dir = file_type.is_dir();
                if file_type.is_file() || file_type.is_symlink() {
                    if !context.ignores.is_ignored_profiled(
                        &git_path,
                        false,
                        context.profile.as_deref_mut(),
                    ) {
                        paths.push(git_path.clone());
                    }
                    return Ok(());
                } else if is_dir {
                    if context.ignores.is_ignored_profiled(
                        &git_path,
                        true,
                        context.profile.as_deref_mut(),
                    ) {
                        return Ok(());
                    }
                    let path = entry.path();
                    if is_same_path(&path, context.git_dir) {
                        return Ok(());
                    }
                    let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                    if let Some(directory_kind) = tracked_directory {
                        if let Some(profile) = context.profile.as_deref_mut() {
                            profile.tracked_dir_prefix_hits += 1;
                            if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                                profile.tracked_skip_worktree_prefix_hits += 1;
                            }
                        }
                    }
                    match context.untracked_mode {
                        StatusUntrackedMode::All => {
                            if tracked_directory.is_none() && is_nested_repository_boundary(&path) {
                                push_untracked_directory(paths, &git_path);
                            } else {
                                collect_status_untracked_paths(context, &path, &git_path, paths)?;
                            }
                        }
                        StatusUntrackedMode::Normal => {
                            if tracked_directory.is_some() {
                                collect_status_untracked_paths(context, &path, &git_path, paths)?;
                            } else if is_nested_repository_boundary(&path) {
                                push_untracked_directory(paths, &git_path);
                            } else if status_untracked_directory_has_file(
                                context, &path, &git_path,
                            )? {
                                push_untracked_directory(paths, &git_path);
                            }
                        }
                        StatusUntrackedMode::None => {}
                    }
                }
                Ok(())
            })();
            git_path.truncate(path_len);
            entry_result?;
        }
        Ok(())
    })();
    context.ignores.truncate(ignore_len);
    result
}

fn stream_status_untracked_paths<T, F>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    emit: &mut F,
) -> Result<StreamControl>
where
    T: StatusTrackedLookup + ?Sized,
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if is_same_path(dir, context.git_dir) {
        return Ok(StreamControl::Continue);
    }
    let ignore_len = context.ignores.patterns.len();
    let mut entries = read_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    entries.sort_by_key(|entry| entry.file_name());
    let result = (|| -> Result<StreamControl> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<StreamControl> {
                if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_exact_hits += 1;
                    }
                    if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                        || tracked_kind == StatusTrackedKind::Gitlink
                    {
                        return Ok(StreamControl::Continue);
                    }
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.file_type_calls += 1;
                    }
                    let file_type = entry.file_type()?;
                    if file_type.is_dir() {
                        let path = entry.path();
                        if !is_same_path(&path, context.git_dir) {
                            if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                .is_stop()
                            {
                                return Ok(StreamControl::Stop);
                            }
                        }
                    }
                    return Ok(StreamControl::Continue);
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type()?;
                let is_dir = file_type.is_dir();
                if file_type.is_file() || file_type.is_symlink() {
                    if !context.ignores.is_ignored_profiled(
                        &git_path,
                        false,
                        context.profile.as_deref_mut(),
                    ) {
                        if emit_status_untracked_path(context, &git_path, emit)?.is_stop() {
                            return Ok(StreamControl::Stop);
                        }
                    }
                    return Ok(StreamControl::Continue);
                } else if is_dir {
                    if context.ignores.is_ignored_profiled(
                        &git_path,
                        true,
                        context.profile.as_deref_mut(),
                    ) {
                        return Ok(StreamControl::Continue);
                    }
                    let path = entry.path();
                    if is_same_path(&path, context.git_dir) {
                        return Ok(StreamControl::Continue);
                    }
                    let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                    if let Some(directory_kind) = tracked_directory {
                        if let Some(profile) = context.profile.as_deref_mut() {
                            profile.tracked_dir_prefix_hits += 1;
                            if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                                profile.tracked_skip_worktree_prefix_hits += 1;
                            }
                        }
                    }
                    match context.untracked_mode {
                        StatusUntrackedMode::All => {
                            if tracked_directory.is_none() && is_nested_repository_boundary(&path) {
                                let directory_len = git_path.len();
                                if git_path.last() != Some(&b'/') {
                                    git_path.push(b'/');
                                }
                                let control =
                                    emit_status_untracked_path(context, &git_path, emit)?;
                                git_path.truncate(directory_len);
                                if control.is_stop() {
                                    return Ok(StreamControl::Stop);
                                }
                            } else {
                                if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                    .is_stop()
                                {
                                    return Ok(StreamControl::Stop);
                                }
                            }
                        }
                        StatusUntrackedMode::Normal => {
                            if tracked_directory.is_some() {
                                if stream_status_untracked_paths(context, &path, &git_path, emit)?
                                    .is_stop()
                                {
                                    return Ok(StreamControl::Stop);
                                }
                            } else if is_nested_repository_boundary(&path)
                                || status_untracked_directory_has_file(context, &path, &git_path)?
                            {
                                let directory_len = git_path.len();
                                if git_path.last() != Some(&b'/') {
                                    git_path.push(b'/');
                                }
                                let control =
                                    emit_status_untracked_path(context, &git_path, emit)?;
                                git_path.truncate(directory_len);
                                if control.is_stop() {
                                    return Ok(StreamControl::Stop);
                                }
                            }
                        }
                        StatusUntrackedMode::None => {}
                    }
                }
                Ok(StreamControl::Continue)
            })();
            git_path.truncate(path_len);
            if entry_result?.is_stop() {
                return Ok(StreamControl::Stop);
            }
        }
        Ok(StreamControl::Continue)
    })();
    context.ignores.truncate(ignore_len);
    result
}

fn count_status_untracked_paths<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
    count: &mut usize,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let ignore_len = context.ignores.patterns.len();
    let entries = read_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    let result = (|| -> Result<()> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<()> {
                if let Some(tracked_kind) = context.tracked.tracked_kind(&git_path) {
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.tracked_exact_hits += 1;
                    }
                    if !matches!(context.untracked_mode, StatusUntrackedMode::All)
                        || tracked_kind == StatusTrackedKind::Gitlink
                    {
                        return Ok(());
                    }
                    if let Some(profile) = context.profile.as_deref_mut() {
                        profile.file_type_calls += 1;
                    }
                    let file_type = entry.file_type()?;
                    if file_type.is_dir() {
                        let path = entry.path();
                        if !is_same_path(&path, context.git_dir) {
                            count_status_untracked_paths(context, &path, &git_path, count)?;
                        }
                    }
                    return Ok(());
                }
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type()?;
                let is_dir = file_type.is_dir();
                if file_type.is_file() || file_type.is_symlink() {
                    if !context.ignores.is_ignored_profiled(
                        &git_path,
                        false,
                        context.profile.as_deref_mut(),
                    ) {
                        *count += 1;
                    }
                    return Ok(());
                } else if is_dir {
                    if context.ignores.is_ignored_profiled(
                        &git_path,
                        true,
                        context.profile.as_deref_mut(),
                    ) {
                        return Ok(());
                    }
                    let path = entry.path();
                    if is_same_path(&path, context.git_dir) {
                        return Ok(());
                    }
                    let tracked_directory = context.tracked.tracked_directory_kind(&git_path);
                    if let Some(directory_kind) = tracked_directory {
                        if let Some(profile) = context.profile.as_deref_mut() {
                            profile.tracked_dir_prefix_hits += 1;
                            if directory_kind == StatusTrackedDirectoryKind::TrackedExcluded {
                                profile.tracked_skip_worktree_prefix_hits += 1;
                            }
                        }
                    }
                    match context.untracked_mode {
                        StatusUntrackedMode::All => {
                            if tracked_directory.is_none() && is_nested_repository_boundary(&path) {
                                *count += 1;
                            } else {
                                count_status_untracked_paths(context, &path, &git_path, count)?;
                            }
                        }
                        StatusUntrackedMode::Normal => {
                            if tracked_directory.is_some() {
                                count_status_untracked_paths(context, &path, &git_path, count)?;
                            } else if is_nested_repository_boundary(&path)
                                || status_untracked_directory_has_file(context, &path, &git_path)?
                            {
                                *count += 1;
                            }
                        }
                        StatusUntrackedMode::None => {}
                    }
                }
                Ok(())
            })();
            git_path.truncate(path_len);
            entry_result?;
        }
        Ok(())
    })();
    context.ignores.truncate(ignore_len);
    result
}

fn emit_status_untracked_path<T, F>(
    context: &mut StatusUntrackedWalk<'_, T>,
    path: &[u8],
    emit: &mut F,
) -> Result<StreamControl>
where
    T: StatusTrackedLookup + ?Sized,
    F: for<'a> FnMut(&'a [u8]) -> Result<StreamControl>,
{
    if let Some(profile) = context.profile.as_deref_mut() {
        profile.untracked_rows += 1;
    }
    emit(path)
}

fn stage0_tracked_directories(index: &Index) -> HashSet<&[u8]> {
    let mut directories = HashSet::new();
    for entry in index
        .entries
        .iter()
        .filter(|entry| entry.stage() == Stage::Normal)
    {
        let path = entry.path.as_bytes();
        for (idx, byte) in path.iter().enumerate() {
            if *byte == b'/' && idx > 0 {
                directories.insert(&path[..idx]);
            }
        }
    }
    directories
}

fn status_untracked_directory_has_file<T: StatusTrackedLookup + ?Sized>(
    context: &mut StatusUntrackedWalk<'_, T>,
    dir: &Path,
    dir_git_path: &[u8],
) -> Result<bool> {
    if is_same_path(dir, context.git_dir) {
        return Ok(false);
    }
    let ignore_len = context.ignores.patterns.len();
    let entries = read_dir_entries_with_ignore_patterns(
        dir,
        dir_git_path,
        context.ignores,
        context.profile.as_deref_mut(),
    )?;
    let result = (|| -> Result<bool> {
        let mut git_path = dir_git_path.to_vec();
        for entry in entries {
            let file_name = entry.file_name();
            if file_name == std::ffi::OsStr::new(".git") {
                continue;
            }
            let path_len = git_path_push_component(&mut git_path, &file_name);
            let entry_result = (|| -> Result<Option<bool>> {
                if let Some(profile) = context.profile.as_deref_mut() {
                    profile.file_type_calls += 1;
                }
                let file_type = entry.file_type()?;
                let is_dir = file_type.is_dir();
                if context.ignores.is_ignored_profiled(
                    &git_path,
                    is_dir,
                    context.profile.as_deref_mut(),
                ) {
                    return Ok(None);
                }
                if file_type.is_file() || file_type.is_symlink() {
                    return Ok(Some(true));
                }
                if is_dir {
                    let path = entry.path();
                    if is_same_path(&path, context.git_dir) {
                        return Ok(None);
                    }
                    if is_nested_repository_boundary(&path) {
                        return Ok(Some(true));
                    }
                    if status_untracked_directory_has_file(context, &path, &git_path)? {
                        return Ok(Some(true));
                    }
                }
                Ok(None)
            })();
            git_path.truncate(path_len);
            if let Some(has_file) = entry_result? {
                return Ok(has_file);
            }
        }
        Ok(false)
    })();
    context.ignores.truncate(ignore_len);
    result
}

fn read_dir_entries_with_ignore_patterns(
    dir: &Path,
    base: &[u8],
    matcher: &mut IgnoreMatcher,
    mut profile: Option<&mut StatusProfileCounters>,
) -> Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    let mut ignore_path = None;
    if let Some(profile) = profile.as_deref_mut() {
        profile.read_dir_calls += 1;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(profile) = profile.as_deref_mut() {
            profile.dir_entries_seen += 1;
        }
        if entry.file_name() == std::ffi::OsStr::new(".gitignore") {
            ignore_path = Some(entry.path());
        }
        entries.push(entry);
    }
    if let Some(path) = ignore_path {
        let mut source = base.to_vec();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(b".gitignore");
        read_ignore_patterns_into_matcher(path, matcher, base, &source);
    }
    Ok(entries)
}

fn push_untracked_directory(paths: &mut Vec<Vec<u8>>, git_path: &[u8]) {
    paths.push(untracked_directory_path(git_path));
}

fn untracked_directory_path(git_path: &[u8]) -> Vec<u8> {
    let mut directory = git_path.to_vec();
    if directory.last() != Some(&b'/') {
        directory.push(b'/');
    }
    directory
}

fn untracked_normal_rollup_path(
    file_path: &[u8],
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Vec<u8> {
    let segments = file_path
        .split(|byte| *byte == b'/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() <= 1 {
        return file_path.to_vec();
    }
    let mut prefix = Vec::new();
    for segment in &segments[..segments.len() - 1] {
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(segment);
        if index_has_path_under(index, &prefix) {
            break;
        }
        if !ignores.is_ignored(&prefix, true) {
            let mut directory = prefix;
            directory.push(b'/');
            return directory;
        }
    }
    file_path.to_vec()
}

fn ignored_traditional_rollup_path(
    root: &Path,
    git_dir: &Path,
    path: &[u8],
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
) -> Result<Vec<u8>> {
    let rolled = untracked_normal_rollup_path(path, index, ignores);
    if rolled == path {
        return Ok(rolled);
    }
    let Some(directory_path) = rolled.strip_suffix(b"/") else {
        return Ok(rolled);
    };
    if ignores.is_ignored(directory_path, true) {
        return Ok(rolled);
    }
    let mut absolute = PathBuf::new();
    set_worktree_path_from_repo_path(root, directory_path, &mut absolute)?;
    if directory_has_file(&absolute, root, git_dir, ignores)? {
        return Ok(path.to_vec());
    }
    Ok(rolled)
}

fn directory_has_file(
    dir: &Path,
    root: &Path,
    git_dir: &Path,
    ignores: &IgnoreMatcher,
) -> Result<bool> {
    if is_same_path(dir, git_dir) {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_embedded_git_internals(root, &path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            continue;
        }
        if metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(true);
        }
        if metadata.is_dir() {
            if is_nested_repository_boundary(&path) {
                continue;
            }
            if directory_has_file(&path, root, git_dir, ignores)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn directory_has_ignored(
    dir: &Path,
    root: &Path,
    git_dir: &Path,
    ignores: &IgnoreMatcher,
) -> Result<bool> {
    if is_same_path(dir, git_dir) {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_same_path(&path, git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if ignores.is_ignored(&git_path, metadata.is_dir()) {
            return Ok(true);
        }
        if metadata.is_dir() && directory_has_ignored(&path, root, git_dir, ignores)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ignored_untracked_paths(
    root: &Path,
    git_dir: &Path,
    index: &BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &IgnoreMatcher,
    directory: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut paths = BTreeSet::new();
    let context = IgnoredUntrackedContext {
        root,
        git_dir,
        index,
        ignores,
        directory,
    };
    collect_ignored_untracked_paths(&context, root, false, &mut paths)?;
    Ok(paths.into_iter().collect())
}

fn ignored_traditional_path_is_empty_directory(root: &Path, path: &[u8]) -> Result<bool> {
    let Some(path) = path.strip_suffix(b"/") else {
        return Ok(false);
    };
    let mut absolute = PathBuf::new();
    set_worktree_path_from_repo_path(root, path, &mut absolute)?;
    match fs::read_dir(&absolute) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(err) if err.kind() == std::io::ErrorKind::NotADirectory => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

struct IgnoredUntrackedContext<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    index: &'a BTreeMap<Vec<u8>, TrackedEntry>,
    ignores: &'a IgnoreMatcher,
    directory: bool,
}

fn collect_ignored_untracked_paths(
    context: &IgnoredUntrackedContext<'_>,
    dir: &Path,
    parent_ignored: bool,
    paths: &mut BTreeSet<Vec<u8>>,
) -> Result<()> {
    if is_same_path(dir, context.git_dir) {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if is_dot_git_entry(&path) {
            continue;
        }
        if is_same_path(&path, context.git_dir) {
            continue;
        }
        let metadata = entry.metadata()?;
        let relative = path.strip_prefix(context.root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        if metadata.is_dir() {
            let ignored = parent_ignored || context.ignores.is_ignored(&git_path, true);
            if ignored && !index_has_path_under(context.index, &git_path) {
                if context.directory {
                    let mut directory_path = git_path;
                    directory_path.push(b'/');
                    paths.insert(directory_path);
                } else {
                    collect_ignored_untracked_paths(context, &path, true, paths)?;
                }
            } else {
                if is_nested_repository_boundary(&path) {
                    continue;
                }
                collect_ignored_untracked_paths(context, &path, ignored, paths)?;
            }
        } else if !context.index.contains_key(&git_path)
            && (metadata.is_file() || metadata.file_type().is_symlink())
            && (parent_ignored || context.ignores.is_ignored(&git_path, false))
        {
            paths.insert(git_path);
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct IgnoreMatcher {
    patterns: Vec<IgnorePattern>,
    buckets: IgnorePatternBuckets,
}

#[derive(Debug, Default)]
struct IgnorePatternBuckets {
    literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    directory_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    literal_path_basename: HashMap<Vec<u8>, Vec<usize>>,
    directory_literal_path_basename: HashMap<Vec<u8>, Vec<usize>>,
    path_suffix_basename: HashMap<Vec<u8>, Vec<usize>>,
    directory_path_suffix_basename: HashMap<Vec<u8>, Vec<usize>>,
    glob_path_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    glob_directory_literal_basename: HashMap<Vec<u8>, Vec<usize>>,
    glob_path_suffix_basename: Vec<usize>,
    glob_path_prefix_basename: Vec<usize>,
    glob_directory_suffix_basename: Vec<usize>,
    glob_directory_prefix_basename: Vec<usize>,
    suffix_basename: HashMap<u8, Vec<usize>>,
    prefix_basename: HashMap<u8, Vec<usize>>,
    other: Vec<usize>,
}

impl IgnorePatternBuckets {
    fn push(&mut self, index: usize, pattern: &IgnorePattern) {
        match pattern.bucket_kind() {
            IgnoreBucketKind::LiteralBasename => self
                .literal_basename
                .entry(pattern.pattern.clone())
                .or_default()
                .push(index),
            IgnoreBucketKind::DirectoryLiteralBasename => self
                .directory_literal_basename
                .entry(pattern.pattern.clone())
                .or_default()
                .push(index),
            IgnoreBucketKind::LiteralPathBasename => self
                .literal_path_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::DirectoryLiteralPathBasename => self
                .directory_literal_path_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::PathSuffixBasename => {
                let suffix = pattern
                    .pattern
                    .strip_prefix(b"**/")
                    .unwrap_or(&pattern.pattern);
                self.path_suffix_basename
                    .entry(path_basename(suffix).to_vec())
                    .or_default()
                    .push(index);
            }
            IgnoreBucketKind::DirectoryPathSuffixBasename => {
                let suffix = pattern
                    .pattern
                    .strip_prefix(b"**/")
                    .unwrap_or(&pattern.pattern);
                self.directory_path_suffix_basename
                    .entry(path_basename(suffix).to_vec())
                    .or_default()
                    .push(index);
            }
            IgnoreBucketKind::GlobPathLiteralBasename => self
                .glob_path_literal_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::GlobDirectoryLiteralBasename => self
                .glob_directory_literal_basename
                .entry(path_basename(&pattern.pattern).to_vec())
                .or_default()
                .push(index),
            IgnoreBucketKind::GlobPathSuffixBasename => self.glob_path_suffix_basename.push(index),
            IgnoreBucketKind::GlobPathPrefixBasename => self.glob_path_prefix_basename.push(index),
            IgnoreBucketKind::GlobDirectorySuffixBasename => {
                self.glob_directory_suffix_basename.push(index)
            }
            IgnoreBucketKind::GlobDirectoryPrefixBasename => {
                self.glob_directory_prefix_basename.push(index)
            }
            IgnoreBucketKind::SuffixBasename => self
                .suffix_basename
                .entry(*pattern.pattern.last().expect("suffix literal is non-empty"))
                .or_default()
                .push(index),
            IgnoreBucketKind::PrefixBasename => self
                .prefix_basename
                .entry(pattern.pattern[0])
                .or_default()
                .push(index),
            IgnoreBucketKind::Other => self.other.push(index),
        }
    }

    fn truncate(&mut self, len: usize) {
        fn truncate_indices(indices: &mut Vec<usize>, len: usize) {
            let keep = indices.partition_point(|index| *index < len);
            indices.truncate(keep);
        }
        for indices in self.literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.literal_path_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_literal_path_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.path_suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.directory_path_suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.glob_path_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.glob_directory_literal_basename.values_mut() {
            truncate_indices(indices, len);
        }
        truncate_indices(&mut self.glob_path_suffix_basename, len);
        truncate_indices(&mut self.glob_path_prefix_basename, len);
        truncate_indices(&mut self.glob_directory_suffix_basename, len);
        truncate_indices(&mut self.glob_directory_prefix_basename, len);
        for indices in self.suffix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        for indices in self.prefix_basename.values_mut() {
            truncate_indices(indices, len);
        }
        truncate_indices(&mut self.other, len);
    }
}

#[derive(Debug)]
struct IgnorePattern {
    base: Vec<u8>,
    pattern: Vec<u8>,
    original: Vec<u8>,
    source: Vec<u8>,
    line_number: usize,
    negated: bool,
    directory_only: bool,
    anchored: bool,
    has_slash: bool,
    /// How `pattern` should be matched against a slash-free segment. Most
    /// `.gitignore` entries are literals or simple `*.ext` / `prefix*` globs, all
    /// of which match without the allocating wildcard DP engine; only genuinely
    /// complex globs fall through to [`wildcard_path_matches`].
    match_kind: MatchKind,
    glob_literal_prefix_len: usize,
}

/// Classification of an [`IgnorePattern`] that lets common shapes skip the
/// general wildcard matcher. Literal/prefix/suffix variants match a slash-free
/// segment; [`MatchKind::PathSuffix`] handles the common `**/literal/path`
/// shape, and the remaining complex patterns defer to the full engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    /// No metacharacters: matches by byte equality.
    Literal,
    /// `*X` with `X` literal: matches a segment ending in `X`.
    Suffix,
    /// `X*` with `X` literal: matches a segment starting with `X`.
    Prefix,
    /// `**/X/Y` with a literal suffix: matches a path ending at `X/Y`.
    PathSuffix,
    /// Anything else: defer to [`wildcard_path_matches`].
    Glob,
}

fn path_basename(path: &[u8]) -> &[u8] {
    path.rsplit(|byte| *byte == b'/').next().unwrap_or(path)
}

fn path_component_has_glob_meta(component: &[u8]) -> bool {
    component
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
}

fn final_component_match_kind(pattern: &[u8]) -> MatchKind {
    classify_ignore_pattern(path_basename(pattern))
}

fn visit_directory_match_components(
    path: &[u8],
    is_dir: bool,
    mut visit: impl FnMut(&[u8]),
) {
    let mut start = 0usize;
    for (index, byte) in path.iter().enumerate() {
        if *byte == b'/' {
            if index > start {
                visit(&path[start..index]);
            }
            start = index + 1;
        }
    }
    if is_dir && start < path.len() {
        visit(&path[start..]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IgnoreBucketKind {
    LiteralBasename,
    DirectoryLiteralBasename,
    LiteralPathBasename,
    DirectoryLiteralPathBasename,
    PathSuffixBasename,
    DirectoryPathSuffixBasename,
    GlobPathLiteralBasename,
    GlobDirectoryLiteralBasename,
    GlobPathSuffixBasename,
    GlobPathPrefixBasename,
    GlobDirectorySuffixBasename,
    GlobDirectoryPrefixBasename,
    SuffixBasename,
    PrefixBasename,
    Other,
}

/// Classify `pattern` for [`MatchKind`]. `*X`/`X*` fast paths require the literal
/// part to be slash-free so that `ends_with`/`starts_with` on a single segment is
/// exactly equivalent to the glob (`*` never crosses `/`).
fn classify_ignore_pattern(pattern: &[u8]) -> MatchKind {
    if let Some(suffix) = pattern.strip_prefix(b"**/")
        && !suffix.is_empty()
        && !suffix
            .iter()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
    {
        return MatchKind::PathSuffix;
    }
    let stars = pattern.iter().filter(|byte| **byte == b'*').count();
    let other_meta = pattern
        .iter()
        .any(|byte| matches!(byte, b'?' | b'[' | b'\\'));
    if stars == 0 && !other_meta {
        return MatchKind::Literal;
    }
    if stars == 1 && !other_meta {
        let literal = if pattern.first() == Some(&b'*') {
            Some((&pattern[1..], MatchKind::Suffix))
        } else if pattern.last() == Some(&b'*') {
            Some((&pattern[..pattern.len() - 1], MatchKind::Prefix))
        } else {
            None
        };
        if let Some((literal, kind)) = literal
            && !literal.is_empty()
            && !literal.contains(&b'/')
        {
            return kind;
        }
    }
    MatchKind::Glob
}

impl IgnoreMatcher {
    fn from_sources(
        root: &Path,
        exclude_standard: bool,
        patterns: &[Vec<u8>],
        per_directory: &[String],
    ) -> Result<Self> {
        let mut matcher = if exclude_standard {
            Self::from_worktree_root(root)?
        } else {
            Self::default()
        };
        matcher.extend_patterns(patterns);
        matcher.extend_per_directory_patterns(root, per_directory)?;
        Ok(matcher)
    }

    /// Builds only the repository-wide ignore sources — `core.excludesFile` (or the
    /// default global) and `$GIT_DIR/info/exclude` — *without* walking the worktree
    /// for `.gitignore`. The caller folds each directory's `.gitignore` into the
    /// matcher as it descends (see [`read_dir_ignore_patterns`]), so status reads
    /// the tree exactly once instead of doing a separate full-tree ignore pass.
    fn from_worktree_base(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut matcher.patterns,
            &[],
            b".git/info/exclude",
        );
        if !read_core_excludes_file(root, &mut matcher.patterns) {
            read_default_global_excludes_file(&mut matcher.patterns);
        }
        matcher.rebuild_buckets();
        Ok(matcher)
    }

    fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        read_ignore_patterns(
            root.join(".git").join("info").join("exclude"),
            &mut matcher.patterns,
            &[],
            b".git/info/exclude",
        );
        if !read_core_excludes_file(root, &mut matcher.patterns) {
            read_default_global_excludes_file(&mut matcher.patterns);
        }
        collect_per_directory_patterns(
            root,
            root,
            &[String::from(".gitignore")],
            &mut matcher.patterns,
        )?;
        matcher.rebuild_buckets();
        Ok(matcher)
    }

    fn extend_patterns(&mut self, patterns: &[Vec<u8>]) {
        for pattern in patterns {
            self.push_raw_pattern(pattern, &[], &[], 0);
        }
    }

    fn extend_per_directory_patterns(&mut self, root: &Path, names: &[String]) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        collect_per_directory_patterns(root, root, names, &mut self.patterns)?;
        self.rebuild_buckets();
        Ok(())
    }

    fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        self.is_ignored_profiled(path, is_dir, None)
    }

    fn match_for(&self, path: &[u8], is_dir: bool) -> Option<&IgnorePattern> {
        self.match_index_for(path, is_dir, None)
            .and_then(|index| self.patterns.get(index))
    }

    fn is_ignored_profiled(
        &self,
        path: &[u8],
        is_dir: bool,
        mut profile: Option<&mut StatusProfileCounters>,
    ) -> bool {
        if let Some(profile) = profile.as_deref_mut() {
            profile.ignore_checks += 1;
        }
        self.match_index_for(path, is_dir, profile)
            .is_some_and(|index| !self.patterns[index].negated)
    }

    fn match_index_for(
        &self,
        path: &[u8],
        is_dir: bool,
        mut profile: Option<&mut StatusProfileCounters>,
    ) -> Option<usize> {
        let basename = path_basename(path);
        let mut best = None;
        if let Some(indices) = self.buckets.literal_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.literal_path_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.path_suffix_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(indices) = self.buckets.glob_path_literal_basename.get(basename) {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        self.match_final_component_candidates(
            &self.buckets.glob_path_suffix_basename,
            MatchKind::Suffix,
            basename,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        self.match_final_component_candidates(
            &self.buckets.glob_path_prefix_basename,
            MatchKind::Prefix,
            basename,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        visit_directory_match_components(path, is_dir, |component| {
            if let Some(indices) = self.buckets.directory_literal_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self
                .buckets
                .directory_literal_path_basename
                .get(component)
            {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self.buckets.directory_path_suffix_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            if let Some(indices) = self.buckets.glob_directory_literal_basename.get(component) {
                self.match_bucket_candidates(
                    indices,
                    path,
                    basename,
                    is_dir,
                    &mut best,
                    &mut profile,
                );
            }
            self.match_final_component_candidates(
                &self.buckets.glob_directory_suffix_basename,
                MatchKind::Suffix,
                component,
                path,
                basename,
                is_dir,
                &mut best,
                &mut profile,
            );
            self.match_final_component_candidates(
                &self.buckets.glob_directory_prefix_basename,
                MatchKind::Prefix,
                component,
                path,
                basename,
                is_dir,
                &mut best,
                &mut profile,
            );
        });
        if let Some(last) = basename.last()
            && let Some(indices) = self.buckets.suffix_basename.get(last)
        {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        if let Some(first) = basename.first()
            && let Some(indices) = self.buckets.prefix_basename.get(first)
        {
            self.match_bucket_candidates(indices, path, basename, is_dir, &mut best, &mut profile);
        }
        self.match_bucket_candidates(
            &self.buckets.other,
            path,
            basename,
            is_dir,
            &mut best,
            &mut profile,
        );
        best
    }

    fn match_bucket_candidates(
        &self,
        indices: &[usize],
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
        best: &mut Option<usize>,
        profile: &mut Option<&mut StatusProfileCounters>,
    ) {
        for &index in indices.iter().rev() {
            if best.is_some_and(|best| index <= best) {
                break;
            }
            let pattern = &self.patterns[index];
            if !pattern.base_matches(path) {
                continue;
            }
            if !pattern.glob_literal_prefix_matches(path, basename, is_dir) {
                continue;
            }
            if let Some(profile) = profile.as_deref_mut() {
                profile.ignore_pattern_tests += 1;
                if pattern.match_kind == MatchKind::Glob {
                    profile.ignore_glob_fallback_tests += 1;
                }
            }
            if pattern.matches_with_basename(path, basename, is_dir) {
                *best = Some(index);
                break;
            }
        }
    }

    fn match_final_component_candidates(
        &self,
        indices: &[usize],
        kind: MatchKind,
        component: &[u8],
        path: &[u8],
        basename: &[u8],
        is_dir: bool,
        best: &mut Option<usize>,
        profile: &mut Option<&mut StatusProfileCounters>,
    ) {
        for &index in indices.iter().rev() {
            if best.is_some_and(|best| index <= best) {
                break;
            }
            let pattern = &self.patterns[index];
            if !pattern.base_matches(path) {
                continue;
            }
            let final_component = path_basename(&pattern.pattern);
            let candidate = match kind {
                MatchKind::Suffix => component.ends_with(&final_component[1..]),
                MatchKind::Prefix => {
                    component.starts_with(&final_component[..final_component.len() - 1])
                }
                _ => false,
            };
            if !candidate {
                continue;
            }
            if !pattern.glob_literal_prefix_matches(path, basename, is_dir) {
                continue;
            }
            if let Some(profile) = profile.as_deref_mut() {
                profile.ignore_pattern_tests += 1;
                if pattern.match_kind == MatchKind::Glob {
                    profile.ignore_glob_fallback_tests += 1;
                }
            }
            if pattern.matches_with_basename(path, basename, is_dir) {
                *best = Some(index);
                break;
            }
        }
    }

    fn push_pattern(&mut self, pattern: IgnorePattern) {
        let index = self.patterns.len();
        self.buckets.push(index, &pattern);
        self.patterns.push(pattern);
    }

    fn push_raw_pattern(&mut self, raw: &[u8], base: &[u8], source: &[u8], line_number: usize) {
        if let Some(pattern) = parse_ignore_pattern(raw, base, source, line_number) {
            self.push_pattern(pattern);
        }
    }

    fn truncate(&mut self, len: usize) {
        if self.patterns.len() == len {
            return;
        }
        self.patterns.truncate(len);
        self.buckets.truncate(len);
    }

    fn rebuild_buckets(&mut self) {
        let mut buckets = IgnorePatternBuckets::default();
        for (index, pattern) in self.patterns.iter().enumerate() {
            buckets.push(index, pattern);
        }
        self.buckets = buckets;
    }
}

/// Decides whether a worktree path is included by a [`SparseCheckout`].
///
/// In [`SparseCheckoutMode::Full`] the sparse patterns are compiled with the
/// same `.gitignore` grammar used elsewhere in this crate ([`IgnorePattern`]);
/// a path is *in cone* when the last matching pattern is positive. In
/// [`SparseCheckoutMode::Cone`] the patterns are reduced to a set of recursive
/// directory prefixes plus a flag for whether top-level files are kept, and
/// inclusion is decided by literal prefix containment.
#[derive(Debug)]
enum SparseMatcher {
    Full { patterns: Vec<IgnorePattern> },
    Cone(ConeMatcher),
}

#[derive(Debug, Default)]
struct ConeMatcher {
    /// `true` when files directly at the repository root are in cone (`/*`).
    root_files: bool,
    /// Directory prefixes (without leading or trailing `/`) whose entire
    /// subtree is in cone, e.g. `dir1/dir2`.
    recursive_dirs: Vec<Vec<u8>>,
    /// Parent directories that are in cone only for their direct files
    /// (the `/dir/*` guard Git emits so intermediate directories keep their
    /// own files). Stored without leading or trailing `/`.
    parent_dirs: Vec<Vec<u8>>,
}

impl SparseMatcher {
    fn new(sparse: &SparseCheckout, mode: SparseCheckoutMode) -> Self {
        let resolved = match mode {
            SparseCheckoutMode::Auto => {
                if patterns_are_cone(&sparse.patterns) {
                    SparseCheckoutMode::Cone
                } else {
                    SparseCheckoutMode::Full
                }
            }
            other => other,
        };
        match resolved {
            SparseCheckoutMode::Cone => SparseMatcher::Cone(ConeMatcher::compile(&sparse.patterns)),
            // `Auto` has been resolved above; everything else is full matching.
            _ => {
                let mut patterns = Vec::new();
                for pattern in &sparse.patterns {
                    push_ignore_pattern(&mut patterns, pattern, &[], b"sparse-checkout", 0);
                }
                SparseMatcher::Full { patterns }
            }
        }
    }

    /// Returns `true` when the given file path should be present in the
    /// worktree under this sparse specification.
    fn includes_file(&self, path: &[u8]) -> bool {
        match self {
            SparseMatcher::Full { patterns } => {
                let mut included = false;
                for pattern in patterns {
                    if pattern.matches(path, false) {
                        included = !pattern.negated;
                    }
                }
                included
            }
            SparseMatcher::Cone(cone) => cone.includes_file(path),
        }
    }
}

impl ConeMatcher {
    fn compile(patterns: &[Vec<u8>]) -> Self {
        let mut matcher = ConeMatcher::default();
        for raw in patterns {
            let line = sparse_clean_line(raw);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            // Negated guards such as `!/*/` and `!/dir/*/` only exist to stop a
            // recursive match from pulling in nested directories; the positive
            // patterns already capture the cone, so we ignore the negations.
            if line.starts_with(b"!") {
                continue;
            }
            if line == b"/*" {
                matcher.root_files = true;
                continue;
            }
            // `/dir/` -> recursive subtree.
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/")
                && !dir.is_empty()
            {
                matcher.recursive_dirs.push(dir.to_vec());
                continue;
            }
            // `/dir/*` -> direct files of `dir` only (parent guard).
            if let Some(rest) = line.strip_prefix(b"/")
                && let Some(dir) = rest.strip_suffix(b"/*")
                && !dir.is_empty()
            {
                matcher.parent_dirs.push(dir.to_vec());
                continue;
            }
        }
        matcher
    }

    fn includes_file(&self, path: &[u8]) -> bool {
        let parent = match path.iter().rposition(|byte| *byte == b'/') {
            Some(index) => &path[..index],
            None => {
                // A path with no slash is a top-level file.
                return self.root_files;
            }
        };
        if self
            .recursive_dirs
            .iter()
            .any(|dir| path_is_under_dir(path, dir))
        {
            return true;
        }
        self.parent_dirs.iter().any(|dir| dir.as_slice() == parent)
    }
}

/// Strips a CR, leading/trailing whitespace, and an optional trailing slash is
/// preserved (cone patterns are slash sensitive) from a raw sparse line.
fn sparse_clean_line(raw: &[u8]) -> &[u8] {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    trim_ascii_whitespace(line)
}

/// Returns `true` when `path` is the directory `dir` itself or lives anywhere
/// beneath it.
fn path_is_under_dir(path: &[u8], dir: &[u8]) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// Heuristic used by [`SparseCheckoutMode::Auto`]: the pattern set is cone
/// shaped when every (non-comment, non-blank) line is one of the restricted
/// cone forms Git emits.
fn patterns_are_cone(patterns: &[Vec<u8>]) -> bool {
    let mut saw_pattern = false;
    for raw in patterns {
        let line = sparse_clean_line(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        saw_pattern = true;
        let body = line.strip_prefix(b"!").unwrap_or(line);
        let is_cone_shaped = body == b"/*"
            || body == b"/*/"
            || (body.starts_with(b"/")
                && (body.ends_with(b"/") || body.ends_with(b"/*"))
                && !sparse_has_glob_meta(body));
        if !is_cone_shaped {
            return false;
        }
    }
    saw_pattern
}

/// Detects glob metacharacters that disqualify a line from cone interpretation.
/// A single trailing `/*` is allowed by the caller and handled separately.
fn sparse_has_glob_meta(body: &[u8]) -> bool {
    let trimmed = body.strip_suffix(b"/*").unwrap_or(body);
    trimmed
        .iter()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\'))
}

fn read_core_excludes_file(root: &Path, patterns: &mut Vec<IgnorePattern>) -> bool {
    let Ok(config) = sley_config::read_repo_config(&root.join(".git"), None) else {
        return false;
    };
    let Some(value) = config.get("core", None, "excludesFile") else {
        return false;
    };
    let path = expand_core_excludes_file(root, value);
    read_ignore_patterns(path, patterns, &[], value.as_bytes());
    true
}

fn expand_core_excludes_file(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    root.join(path)
}

fn read_default_global_excludes_file(patterns: &mut Vec<IgnorePattern>) {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        && !config_home.is_empty()
    {
        let path = PathBuf::from(config_home).join("git").join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
        return;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home)
            .join(".config")
            .join("git")
            .join("ignore");
        let source = path.to_string_lossy().into_owned();
        read_ignore_patterns(path, patterns, &[], source.as_bytes());
    }
}

fn collect_per_directory_patterns(
    root: &Path,
    dir: &Path,
    names: &[String],
    patterns: &mut Vec<IgnorePattern>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_per_directory_patterns(root, &path, names, patterns)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !names.iter().any(|name| name == file_name) {
            continue;
        }
        let parent = path.parent().unwrap_or(root);
        let relative = parent.strip_prefix(root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", parent.display()))
        })?;
        let base = git_path_bytes(relative)?;
        let mut source = base.clone();
        if !source.is_empty() {
            source.push(b'/');
        }
        source.extend_from_slice(file_name.as_bytes());
        read_ignore_patterns(&path, patterns, &base, &source);
    }
    Ok(())
}

fn read_ignore_patterns(
    path: impl AsRef<Path>,
    patterns: &mut Vec<IgnorePattern>,
    base: &[u8],
    source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    for (line, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        push_ignore_pattern(patterns, raw, base, source, line + 1);
    }
}

fn read_ignore_patterns_into_matcher(
    path: impl AsRef<Path>,
    matcher: &mut IgnoreMatcher,
    base: &[u8],
    source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    for (line, raw) in contents.split(|byte| *byte == b'\n').enumerate() {
        matcher.push_raw_pattern(raw, base, source, line + 1);
    }
}

fn push_ignore_pattern(
    patterns: &mut Vec<IgnorePattern>,
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) {
    if let Some(pattern) = parse_ignore_pattern(raw, base, source, line_number) {
        patterns.push(pattern);
    }
}

fn parse_ignore_pattern(
    raw: &[u8],
    base: &[u8],
    source: &[u8],
    line_number: usize,
) -> Option<IgnorePattern> {
    let mut line = raw.strip_suffix(b"\r").unwrap_or(raw).to_vec();
    normalize_ignore_trailing_spaces(&mut line);
    let original = line.clone();
    let mut line = line.as_slice();
    if line.is_empty() || line.starts_with(b"#") {
        return None;
    }
    let negated = if line.starts_with(b"\\#") || line.starts_with(b"\\!") {
        line = &line[1..];
        false
    } else if let Some(pattern) = line.strip_prefix(b"!") {
        line = pattern;
        true
    } else {
        false
    };
    let directory_only = line.ends_with(b"/");
    let pattern = if directory_only {
        line.strip_suffix(b"/").unwrap_or(line)
    } else {
        line
    };
    let (anchored, pattern) = if let Some(pattern) = pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, pattern)
    };
    // A leading `**/` followed by a slash-free segment is, per gitignore,
    // identical to the bare segment ("match in all directories"): `**/Pods` ≡
    // `Pods`, `**/*.jks` ≡ `*.jks`. Collapse it so the pattern matches the
    // basename directly (a literal/suffix compare) instead of paying for the
    // `**` wildcard engine on the full path — verified against `git check-ignore`.
    let pattern = match pattern.strip_prefix(b"**/") {
        Some(rest) if !rest.is_empty() && !rest.contains(&b'/') => rest,
        _ => pattern,
    };
    if pattern.is_empty() {
        return None;
    }
    let match_kind = classify_ignore_pattern(pattern);
    let glob_literal_prefix_len = if match_kind == MatchKind::Glob {
        pattern
            .iter()
            .position(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
            .unwrap_or(pattern.len())
    } else {
        0
    };
    Some(IgnorePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        original,
        source: source.to_vec(),
        line_number,
        negated,
        directory_only,
        anchored,
        has_slash: pattern.contains(&b'/'),
        match_kind,
        glob_literal_prefix_len,
    })
}

fn normalize_ignore_trailing_spaces(line: &mut Vec<u8>) {
    while line.last() == Some(&b' ') {
        let space_index = line.len() - 1;
        let backslashes = line[..space_index]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            line.remove(space_index - 1);
            break;
        }
        line.pop();
    }
}

impl IgnorePattern {
    fn bucket_kind(&self) -> IgnoreBucketKind {
        if self.match_kind == MatchKind::PathSuffix {
            return if self.directory_only {
                IgnoreBucketKind::DirectoryPathSuffixBasename
            } else {
                IgnoreBucketKind::PathSuffixBasename
            };
        }
        if (self.anchored || self.has_slash) && self.match_kind == MatchKind::Literal {
            return if self.directory_only {
                IgnoreBucketKind::DirectoryLiteralPathBasename
            } else {
                IgnoreBucketKind::LiteralPathBasename
            };
        }
        if self.has_slash
            && self.match_kind == MatchKind::Glob
            && !self.directory_only
            && !path_component_has_glob_meta(path_basename(&self.pattern))
        {
            return IgnoreBucketKind::GlobPathLiteralBasename;
        }
        if self.has_slash
            && self.match_kind == MatchKind::Glob
            && self.directory_only
            && !path_component_has_glob_meta(path_basename(&self.pattern))
        {
            return IgnoreBucketKind::GlobDirectoryLiteralBasename;
        }
        if self.has_slash && self.match_kind == MatchKind::Glob {
            return match (
                self.directory_only,
                final_component_match_kind(&self.pattern),
            ) {
                (false, MatchKind::Suffix) => IgnoreBucketKind::GlobPathSuffixBasename,
                (false, MatchKind::Prefix) => IgnoreBucketKind::GlobPathPrefixBasename,
                (true, MatchKind::Suffix) => IgnoreBucketKind::GlobDirectorySuffixBasename,
                (true, MatchKind::Prefix) => IgnoreBucketKind::GlobDirectoryPrefixBasename,
                _ => IgnoreBucketKind::Other,
            };
        }
        if self.anchored || self.has_slash {
            return IgnoreBucketKind::Other;
        }
        match (self.directory_only, self.match_kind) {
            (false, MatchKind::Literal) => IgnoreBucketKind::LiteralBasename,
            (true, MatchKind::Literal) => IgnoreBucketKind::DirectoryLiteralBasename,
            (false, MatchKind::Suffix) => IgnoreBucketKind::SuffixBasename,
            (false, MatchKind::Prefix) => IgnoreBucketKind::PrefixBasename,
            _ => IgnoreBucketKind::Other,
        }
    }

    fn base_matches(&self, path: &[u8]) -> bool {
        if self.base.is_empty() {
            return true;
        }
        path.strip_prefix(self.base.as_slice())
            .is_some_and(|rest| rest.starts_with(b"/"))
    }

    fn to_match(&self) -> IgnoreMatch {
        IgnoreMatch {
            source: self.source.clone(),
            line_number: self.line_number,
            pattern: self.original.clone(),
            ignored: !self.negated,
        }
    }

    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        let basename = path_basename(path);
        self.matches_with_basename(path, basename, is_dir)
    }

    fn glob_literal_prefix_matches(&self, path: &[u8], basename: &[u8], is_dir: bool) -> bool {
        if self.match_kind != MatchKind::Glob {
            return true;
        }
        if self.glob_literal_prefix_len == 0 {
            return true;
        }
        let prefix = &self.pattern[..self.glob_literal_prefix_len];
        let scoped_path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.anchored || self.has_slash {
            return scoped_path.starts_with(prefix);
        }
        if self.directory_only && !is_dir {
            return true;
        }
        basename.starts_with(prefix)
    }

    fn matches_with_basename(&self, path: &[u8], basename: &[u8], is_dir: bool) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.directory_only {
            return self.matches_directory(path, is_dir);
        }
        if self.anchored || self.has_slash {
            return self.match_segment(path);
        }
        self.match_segment(basename)
    }

    fn matches_directory(&self, path: &[u8], is_dir: bool) -> bool {
        if self.anchored || self.has_slash {
            if is_dir && self.match_path(path) {
                return true;
            }
            // For a *file* path, a directory-only pattern can only apply
            // through an *ancestor* directory of the file: the leaf is matched
            // only because it lives inside a directory the pattern excludes
            // (e.g. `/tmp-*/` excludes `tmp-info-only`, so `tmp-info-only/x`
            // is excluded too). Upstream git models this through directory
            // traversal — `last_matching_pattern` skips a MUSTBEDIR pattern for
            // a non-directory leaf (`dtype != DT_DIR`), and a file is excluded
            // only when one of its parent directories is excluded.
            //
            // A *negated* directory-only pattern (`!data/**/`) re-includes a
            // directory but, per git, does NOT re-include the files inside it
            // (git's docs: "it is not possible to re-include a file if a parent
            // directory of that file is excluded" — re-including the dir with
            // `!dir/` still requires an explicit `!dir/*` to reach its files).
            // So a negated directory-only pattern must never match a file via
            // its ancestor, otherwise it wrongly wins the leaf scan and
            // un-ignores a file that an earlier positive pattern ignored
            // (t0008-ignores "directories and ** matches": `data/**` +
            // `!data/**/` must leave `data/data1/file1` ignored).
            if self.negated {
                return false;
            }
            return path
                .iter()
                .enumerate()
                .any(|(idx, byte)| *byte == b'/' && self.match_path(&path[..idx]));
        }
        let mut components = path.split(|byte| *byte == b'/').peekable();
        while let Some(component) = components.next() {
            if self.match_segment(component) && (is_dir || components.peek().is_some()) {
                return true;
            }
        }
        false
    }

    fn match_path(&self, value: &[u8]) -> bool {
        match self.match_kind {
            MatchKind::Literal => self.pattern == value,
            MatchKind::Suffix => !value.contains(&b'/') && value.ends_with(&self.pattern[1..]),
            MatchKind::Prefix => {
                !value.contains(&b'/') && value.starts_with(&self.pattern[..self.pattern.len() - 1])
            }
            MatchKind::PathSuffix => {
                let suffix = &self.pattern[3..];
                value
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(b"/"))
            }
            MatchKind::Glob => wildcard_path_matches(&self.pattern, value),
        }
    }

    /// Match a slash-free `value` (a basename or path component) against this
    /// pattern. Literal and simple `*X`/`X*` patterns resolve with a direct
    /// comparison; only complex globs pay for the allocating wildcard engine.
    fn match_segment(&self, value: &[u8]) -> bool {
        self.match_path(value)
    }
}

thread_local! {
    /// Reused dynamic-programming scratch for [`wildcard_path_matches`]. Flat
    /// `(pattern.len()+1) * (value.len()+1)` grid of memoised results, kept across
    /// calls so the hot ignore/attribute matching loop never reallocates.
    static WILDCARD_MEMO: RefCell<Vec<Option<bool>>> = const { RefCell::new(Vec::new()) };
}

fn wildcard_path_matches(pattern: &[u8], value: &[u8]) -> bool {
    let stride = value.len() + 1;
    let cells = (pattern.len() + 1) * stride;
    WILDCARD_MEMO.with_borrow_mut(|memo| {
        // One reused allocation; clearing then resizing fills the grid with `None`.
        memo.clear();
        memo.resize(cells, None);
        wildcard_path_matches_from(pattern, value, 0, 0, memo, stride)
    })
}

fn wildcard_path_matches_from(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let cell = pattern_index * stride + value_index;
    if let Some(cached) = memo[cell] {
        return cached;
    }
    let matched = if pattern_index == pattern.len() {
        value_index == value.len()
    } else {
        match pattern[pattern_index] {
            b'*' if pattern.get(pattern_index + 1) == Some(&b'*') => wildcard_double_star_matches(
                pattern,
                value,
                pattern_index,
                value_index,
                memo,
                stride,
            ),
            b'*' => {
                if wildcard_path_matches_from(
                    pattern,
                    value,
                    pattern_index + 1,
                    value_index,
                    memo,
                    stride,
                ) {
                    true
                } else {
                    let mut next = value_index;
                    while next < value.len() && value[next] != b'/' {
                        next += 1;
                        if wildcard_path_matches_from(
                            pattern,
                            value,
                            pattern_index + 1,
                            next,
                            memo,
                            stride,
                        ) {
                            return true;
                        }
                    }
                    false
                }
            }
            b'?' => {
                value_index < value.len()
                    && value[value_index] != b'/'
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            b'[' => {
                if value_index < value.len() && value[value_index] != b'/' {
                    if let Some((class_matches, next_pattern_index)) =
                        wildcard_class_matches(pattern, pattern_index, value[value_index])
                    {
                        class_matches
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                next_pattern_index,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    } else {
                        value[value_index] == b'['
                            && wildcard_path_matches_from(
                                pattern,
                                value,
                                pattern_index + 1,
                                value_index + 1,
                                memo,
                                stride,
                            )
                    }
                } else {
                    false
                }
            }
            b'\\' if pattern_index + 1 < pattern.len() => {
                value_index < value.len()
                    && pattern[pattern_index + 1] == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 2,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
            literal => {
                value_index < value.len()
                    && literal == value[value_index]
                    && wildcard_path_matches_from(
                        pattern,
                        value,
                        pattern_index + 1,
                        value_index + 1,
                        memo,
                        stride,
                    )
            }
        }
    };
    memo[cell] = Some(matched);
    matched
}

fn wildcard_double_star_matches(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Option<bool>],
    stride: usize,
) -> bool {
    let after_stars = pattern_index + 2;
    if pattern.get(after_stars) == Some(&b'/') {
        if wildcard_path_matches_from(pattern, value, after_stars + 1, value_index, memo, stride) {
            return true;
        }
        for next in value_index..value.len() {
            if value[next] == b'/'
                && wildcard_path_matches_from(
                    pattern,
                    value,
                    after_stars + 1,
                    next + 1,
                    memo,
                    stride,
                )
            {
                return true;
            }
        }
        return false;
    }
    for next in value_index..=value.len() {
        if wildcard_path_matches_from(pattern, value, after_stars, next, memo, stride) {
            return true;
        }
    }
    false
}

fn wildcard_class_matches(pattern: &[u8], start: usize, value: u8) -> Option<(bool, usize)> {
    let mut index = start + 1;
    let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
    if negated {
        index += 1;
    }
    let class_start = index;
    let end = pattern[class_start..]
        .iter()
        .position(|byte| *byte == b']')
        .map(|position| class_start + position)?;
    if end == class_start {
        return None;
    }
    let mut matched = false;
    while index < end {
        if index + 2 < end && pattern[index + 1] == b'-' {
            let lower = pattern[index].min(pattern[index + 2]);
            let upper = pattern[index].max(pattern[index + 2]);
            matched |= lower <= value && value <= upper;
            index += 3;
        } else {
            matched |= pattern[index] == value;
            index += 1;
        }
    }
    Some((if negated { !matched } else { matched }, end + 1))
}

#[derive(Debug, Default)]
struct AttributeMatcher {
    patterns: Vec<AttributePattern>,
    attribute_order: BTreeMap<Vec<u8>, usize>,
    macros: BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
}

#[derive(Debug)]
struct AttributePattern {
    base: Vec<u8>,
    pattern: Vec<u8>,
    anchored: bool,
    has_slash: bool,
    assignments: Vec<AttributeAssignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttributeAssignment {
    attribute: Vec<u8>,
    state: Option<AttributeState>,
}

impl AttributeMatcher {
    fn from_worktree_root(root: &Path) -> Result<Self> {
        let mut matcher = Self::default();
        if !matcher.read_configured_attributes(root) {
            matcher.read_default_global_attributes();
        }
        collect_attribute_patterns(root, root, &mut matcher)?;
        read_attribute_patterns(
            root.join(".git").join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
        );
        Ok(matcher)
    }

    /// Builds only the repository-wide attribute sources — `core.attributesFile`
    /// (or the default global) and `$GIT_DIR/info/attributes` — *without* walking
    /// the worktree for `.gitattributes`. The caller is expected to fold each
    /// directory's `.gitattributes` into the matcher as it descends (see
    /// [`read_dir_attribute_patterns`]), so status/diff read the tree exactly once
    /// instead of doing a separate full-tree attribute pass. Lower-priority sources
    /// are added first, so in-tree patterns added during the walk take precedence —
    /// matching git's lookup order.
    fn from_worktree_base(root: &Path) -> Self {
        let mut matcher = Self::default();
        if !matcher.read_configured_attributes(root) {
            matcher.read_default_global_attributes();
        }
        read_attribute_patterns(
            root.join(".git").join("info").join("attributes"),
            &mut matcher,
            &[],
            b".git/info/attributes",
        );
        matcher
    }

    fn attributes_for_path(
        &self,
        path: &[u8],
        requested: &[Vec<u8>],
        all: bool,
    ) -> Vec<AttributeCheck> {
        let mut states = BTreeMap::<Vec<u8>, Option<AttributeState>>::new();
        for pattern in &self.patterns {
            if !pattern.matches(path) {
                continue;
            }
            for assignment in &pattern.assignments {
                states.insert(assignment.attribute.clone(), assignment.state.clone());
            }
        }
        if all {
            let mut checks = states
                .into_iter()
                .filter_map(|(attribute, state)| {
                    state.map(|state| AttributeCheck {
                        attribute,
                        state: Some(state),
                    })
                })
                .collect::<Vec<_>>();
            checks.sort_by(|left, right| {
                attribute_all_rank(&left.attribute, &self.attribute_order)
                    .cmp(&attribute_all_rank(&right.attribute, &self.attribute_order))
                    .then_with(|| left.attribute.cmp(&right.attribute))
            });
            return checks;
        }
        requested
            .iter()
            .map(|attribute| AttributeCheck {
                attribute: attribute.clone(),
                state: states.get(attribute).cloned().flatten(),
            })
            .collect()
    }

    fn push_attribute_order(&mut self, attribute: &[u8]) {
        let next = self.attribute_order.len();
        self.attribute_order
            .entry(attribute.to_vec())
            .or_insert(next);
    }

    fn read_configured_attributes(&mut self, root: &Path) -> bool {
        let Ok(config) = sley_config::read_repo_config(&root.join(".git"), None) else {
            return false;
        };
        let Some(value) = config.get("core", None, "attributesFile") else {
            return false;
        };
        let path = expand_core_excludes_file(root, value);
        read_attribute_patterns(path, self, &[], value.as_bytes());
        true
    }

    fn read_default_global_attributes(&mut self) {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
            && !config_home.is_empty()
        {
            let path = PathBuf::from(config_home).join("git").join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes());
            return;
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home)
                .join(".config")
                .join("git")
                .join("attributes");
            let source = path.to_string_lossy().into_owned();
            read_attribute_patterns(path, self, &[], source.as_bytes());
        }
    }
}

fn read_dir_ignore_patterns_for_base(
    dir: &Path,
    base: &[u8],
    matcher: &mut IgnoreMatcher,
) -> Result<()> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitignore");
    read_ignore_patterns_into_matcher(dir.join(".gitignore"), matcher, base, &source);
    Ok(())
}

/// Fold `dir`'s `.gitattributes` (if any) into `matcher`, scoped to `dir`'s path
/// within `root`. Used both by the eager full-tree pass and by the status/diff
/// worktree walk as it descends, so the tree is read for attributes exactly once.
fn read_dir_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let relative = dir.strip_prefix(root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", dir.display()))
    })?;
    let base = git_path_bytes(relative)?;
    read_dir_attribute_patterns_for_base(dir, &base, matcher)
}

fn read_dir_attribute_patterns_for_base(
    dir: &Path,
    base: &[u8],
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let mut source = base.to_vec();
    if !source.is_empty() {
        source.push(b'/');
    }
    source.extend_from_slice(b".gitattributes");
    read_attribute_patterns(dir.join(".gitattributes"), matcher, base, &source);
    Ok(())
}

fn collect_attribute_patterns(
    root: &Path,
    dir: &Path,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    read_dir_attribute_patterns(root, dir, matcher)?;

    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        if entry.metadata()?.is_dir() {
            collect_attribute_patterns(root, &path, matcher)?;
        }
    }
    Ok(())
}

fn read_attribute_patterns(
    path: impl AsRef<Path>,
    matcher: &mut AttributeMatcher,
    base: &[u8],
    _source: &[u8],
) {
    let Ok(contents) = fs::read(path) else {
        return;
    };
    read_attribute_patterns_from_bytes(&contents, matcher, base);
}

fn read_attribute_patterns_from_bytes(
    contents: &[u8],
    matcher: &mut AttributeMatcher,
    base: &[u8],
) {
    for raw in contents.split(|byte| *byte == b'\n') {
        push_attribute_pattern(matcher, raw, base);
    }
}

fn collect_attribute_patterns_from_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    base: Vec<u8>,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let object = read_expected_object(db, tree_oid, ObjectType::Tree)?;
    let mut entries = Tree::parse(format, &object.body)?.entries;
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    for entry in &entries {
        if entry.name == b".gitattributes" && tree_entry_object_type(entry.mode) == ObjectType::Blob
        {
            let object = db.read_object(&entry.oid).map_err(|err| {
                expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob)
            })?;
            if object.object_type == ObjectType::Blob {
                read_attribute_patterns_from_bytes(&object.body, matcher, &base);
            }
        }
    }
    for entry in entries {
        if tree_entry_object_type(entry.mode) != ObjectType::Tree {
            continue;
        }
        let mut child_base = base.clone();
        if !child_base.is_empty() {
            child_base.push(b'/');
        }
        child_base.extend_from_slice(entry.name.as_bytes());
        collect_attribute_patterns_from_tree(db, format, &entry.oid, child_base, matcher)?;
    }
    Ok(())
}

fn collect_attribute_patterns_from_index(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(());
    }
    let mut entries = Index::parse(&fs::read(index_path)?, format)?.entries;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    for entry in entries {
        let is_attributes_file =
            entry.path == b".gitattributes" || entry.path.as_bytes().ends_with(b"/.gitattributes");
        if index_entry_stage(&entry) != 0
            || tree_entry_object_type(entry.mode) != ObjectType::Blob
            || !is_attributes_file
        {
            continue;
        }
        let base = match entry.path.as_bytes().strip_suffix(b".gitattributes") {
            Some(b"") => Vec::new(),
            Some(parent) => parent.strip_suffix(b"/").unwrap_or(parent).to_vec(),
            None => continue,
        };
        let object = db
            .read_object(&entry.oid)
            .map_err(|err| expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob))?;
        if object.object_type == ObjectType::Blob {
            read_attribute_patterns_from_bytes(&object.body, matcher, &base);
        }
    }
    Ok(())
}

fn push_attribute_pattern(matcher: &mut AttributeMatcher, raw: &[u8], base: &[u8]) {
    let line = raw.strip_suffix(b"\r").unwrap_or(raw);
    let line = trim_ascii_whitespace(line);
    if line.is_empty() || line.starts_with(b"#") {
        return;
    }
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let Some(raw_pattern) = fields.next() else {
        return;
    };
    if let Some(macro_name) = raw_pattern.strip_prefix(b"[attr]") {
        if macro_name.is_empty() {
            return;
        }
        let mut assignments = vec![AttributeAssignment {
            attribute: macro_name.to_vec(),
            state: Some(AttributeState::Set),
        }];
        for field in fields {
            push_attribute_assignments(&mut assignments, field, &matcher.macros);
        }
        for assignment in &assignments {
            matcher.push_attribute_order(&assignment.attribute);
        }
        matcher.macros.insert(macro_name.to_vec(), assignments);
        return;
    }
    let mut assignments = Vec::new();
    for field in fields {
        push_attribute_assignments(&mut assignments, field, &matcher.macros);
    }
    if assignments.is_empty() {
        return;
    }
    for assignment in &assignments {
        matcher.push_attribute_order(&assignment.attribute);
    }
    let (anchored, pattern) = if let Some(pattern) = raw_pattern.strip_prefix(b"/") {
        (true, pattern)
    } else {
        (false, raw_pattern)
    };
    if pattern.is_empty() {
        return;
    }
    matcher.patterns.push(AttributePattern {
        base: base.to_vec(),
        pattern: pattern.to_vec(),
        anchored,
        has_slash: pattern.contains(&b'/'),
        assignments,
    });
}

fn push_attribute_assignments(
    assignments: &mut Vec<AttributeAssignment>,
    field: &[u8],
    macros: &BTreeMap<Vec<u8>, Vec<AttributeAssignment>>,
) {
    if let Some(macro_assignments) = macros.get(field) {
        assignments.extend(macro_assignments.iter().cloned());
        return;
    }
    if field == b"binary" {
        assignments.push(AttributeAssignment {
            attribute: b"binary".to_vec(),
            state: Some(AttributeState::Set),
        });
        assignments.push(AttributeAssignment {
            attribute: b"diff".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"merge".to_vec(),
            state: Some(AttributeState::Unset),
        });
        assignments.push(AttributeAssignment {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        });
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"-") {
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Unset),
            });
        }
        return;
    }
    if let Some(attribute) = field.strip_prefix(b"!") {
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: None,
            });
        }
        return;
    }
    if let Some(equal) = field.iter().position(|byte| *byte == b'=') {
        let attribute = &field[..equal];
        let value = &field[equal + 1..];
        if !attribute.is_empty() {
            assignments.push(AttributeAssignment {
                attribute: attribute.to_vec(),
                state: Some(AttributeState::Value(value.to_vec())),
            });
        }
        return;
    }
    assignments.push(AttributeAssignment {
        attribute: field.to_vec(),
        state: Some(AttributeState::Set),
    });
}

fn attribute_all_rank(
    attribute: &[u8],
    order: &BTreeMap<Vec<u8>, usize>,
) -> (usize, usize, Vec<u8>) {
    let rank = match attribute {
        b"binary" => 0,
        b"diff" => 1,
        b"merge" => 2,
        b"text" => 3,
        b"eol" => 5,
        _ => 4,
    };
    let order = order.get(attribute).copied().unwrap_or(usize::MAX);
    (rank, order, attribute.to_vec())
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

impl AttributePattern {
    fn matches(&self, path: &[u8]) -> bool {
        let path = if self.base.is_empty() {
            path
        } else {
            let Some(rest) = path
                .strip_prefix(self.base.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            else {
                return false;
            };
            rest
        };
        if self.anchored || self.has_slash {
            return wildcard_path_matches(&self.pattern, path);
        }
        path.rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|basename| wildcard_path_matches(&self.pattern, basename))
    }
}

// ---------------------------------------------------------------------------
// Content filtering on the blob <-> worktree boundary
//
// Git runs two kinds of conversion when content crosses between the worktree
// and the object database:
//
//   * the line-ending / `core.autocrlf` conversion (driven by the `text`,
//     `eol` attributes and the `core.autocrlf` / `core.eol` config), and
//   * the long-running `filter.<name>.clean` / `.smudge` driver filters
//     (selected by the `filter=<name>` attribute and configured commands).
//
// "clean" runs on the way *into* the object store (worktree -> blob), e.g. on
// `git add` / `git hash-object -w`. "smudge" runs on the way *out* (blob ->
// worktree), e.g. on checkout / restore. The driver filter, when present,
// wraps the EOL conversion: on clean git first runs the configured `clean`
// command and then applies CRLF->LF normalization; on smudge git first applies
// LF->CRLF and then runs the `smudge` command.
// ---------------------------------------------------------------------------

/// The line-ending conversion that applies to a path, derived from its
/// attributes and the repository config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EolConversion {
    /// No conversion: binary content, or text with `core.autocrlf=false` and no
    /// `eol`/`text=auto` request to add carriage returns.
    None,
    /// Normalize to LF on clean; no carriage returns on smudge (`eol=lf`, or
    /// `core.autocrlf=input`).
    Lf,
    /// Normalize to LF on clean; emit CRLF on smudge (`eol=crlf`, or
    /// `core.autocrlf=true`).
    Crlf,
}

/// How git should decide whether a path is text for the purpose of EOL
/// conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDecision {
    /// `-text` / `binary`: never convert.
    Binary,
    /// `text` is set explicitly: always treat as text.
    Text,
    /// `text=auto` (or implied by `core.autocrlf`): treat as text unless the
    /// content looks binary.
    Auto,
    /// No opinion from attributes or config: leave content untouched.
    Unspecified,
}

/// The fully resolved set of conversions that apply to a single path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentFilterPlan {
    text: TextDecision,
    /// The conversion to apply when `text` resolves to "this is text".
    eol: EolConversion,
    /// `filter.<name>` driver, if assigned via attributes and configured.
    driver: Option<FilterDriver>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterDriver {
    name: Vec<u8>,
    clean: Option<String>,
    smudge: Option<String>,
    required: bool,
}

/// Decode one crlf-family attribute (`text` or its legacy alias `crlf`) into a
/// text decision, plus whether the value form forced an EOL direction.
///
/// Mirrors git's `git_path_check_crlf` (convert.c): a *set* attribute is text,
/// an *unset* one is binary, `=auto` is auto, `=input` forces LF while still
/// counting as text, and any other value is "undefined" — i.e. no opinion, so
/// the caller falls through to the next source (the `crlf` alias, then config).
fn decode_crlf_family_attribute(state: Option<&AttributeState>) -> (TextDecision, EolConversion) {
    match state {
        Some(AttributeState::Set) => (TextDecision::Text, EolConversion::None),
        Some(AttributeState::Unset) => (TextDecision::Binary, EolConversion::None),
        Some(AttributeState::Value(value)) if value == b"auto" => {
            (TextDecision::Auto, EolConversion::None)
        }
        // `crlf=input` / `text=input`: text content normalized to LF (no CR on
        // smudge), exactly like `core.autocrlf=input`.
        Some(AttributeState::Value(value)) if value == b"input" => {
            (TextDecision::Text, EolConversion::Lf)
        }
        // `=<other>` is CRLF_UNDEFINED in git for the `crlf` alias: no opinion.
        _ => (TextDecision::Unspecified, EolConversion::None),
    }
}

impl ContentFilterPlan {
    /// Build the plan for `path` from the parsed attributes and repo config.
    fn resolve(config: &GitConfig, checks: &[AttributeCheck]) -> Self {
        let text_attr = checks.iter().find(|check| check.attribute == b"text");
        let crlf_attr = checks.iter().find(|check| check.attribute == b"crlf");
        let eol_attr = checks.iter().find(|check| check.attribute == b"eol");
        let filter_attr = checks.iter().find(|check| check.attribute == b"filter");

        // Resolve the eol attribute first; `eol=crlf|lf` also forces text.
        let eol_value = eol_attr.and_then(|check| match &check.state {
            Some(AttributeState::Value(value)) => Some(value.clone()),
            _ => None,
        });

        // The `text` attribute decides first; only when it is unspecified does
        // git consult the legacy `crlf` alias (convert.c `convert_attrs`).
        let mut forced_eol = EolConversion::None;
        let mut text = match text_attr.map(|check| &check.state) {
            Some(Some(AttributeState::Set)) => TextDecision::Text,
            Some(Some(AttributeState::Unset)) => TextDecision::Binary,
            Some(Some(AttributeState::Value(value))) if value == b"auto" => TextDecision::Auto,
            Some(Some(AttributeState::Value(value))) if value == b"input" => {
                forced_eol = EolConversion::Lf;
                TextDecision::Text
            }
            // `text=<other>` is treated by git as a set text attribute.
            Some(Some(AttributeState::Value(_))) => TextDecision::Text,
            // `!text` (unspecified) or no text attribute: fall through to `crlf`.
            _ => {
                let (decision, eol) =
                    decode_crlf_family_attribute(crlf_attr.and_then(|check| check.state.as_ref()));
                forced_eol = eol;
                decision
            }
        };

        // A concrete `eol` attribute implies the path is text even when `text`
        // was left unspecified (git: `eol` without `text` is treated as
        // `text=auto`-ish; upstream forces conversion). We honour eol only when
        // text is not explicitly binary.
        let eol = match (&text, eol_value.as_deref()) {
            (TextDecision::Binary, _) => EolConversion::None,
            (_, Some(b"crlf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Crlf
            }
            (_, Some(b"lf")) => {
                if text == TextDecision::Unspecified {
                    text = TextDecision::Text;
                }
                EolConversion::Lf
            }
            // No explicit `eol` attribute, but `text=input`/`crlf=input` already
            // forced the LF direction (git's CRLF_TEXT_INPUT). Honour it over the
            // config-derived default.
            _ if forced_eol == EolConversion::Lf => EolConversion::Lf,
            // No eol attribute: derive direction from config.
            _ => eol_from_config(config),
        };

        // When the path is text but neither `eol` nor `core.autocrlf`/`core.eol`
        // asked for carriage returns, we still normalize to LF on clean. That is
        // modelled by `EolConversion::Lf` (clean strips CR, smudge adds none).
        let eol = match (&text, eol) {
            (TextDecision::Text | TextDecision::Auto, EolConversion::None) => EolConversion::Lf,
            (_, eol) => eol,
        };

        // If config does not enable autocrlf and there is no eol/text opinion,
        // there is genuinely nothing to do.
        let text = match (text, eol_attr.is_some()) {
            (TextDecision::Unspecified, _) => {
                // Without any text/eol attribute, only `core.autocrlf` can make a
                // path eligible, and then it behaves like `text=auto`.
                if autocrlf_enabled(config) {
                    TextDecision::Auto
                } else {
                    TextDecision::Unspecified
                }
            }
            (text, _) => text,
        };

        let driver = resolve_filter_driver(config, filter_attr);

        ContentFilterPlan { text, eol, driver }
    }

    /// Whether EOL conversion should run for the given content.
    fn convert_eol(&self, content: &[u8]) -> bool {
        match self.text {
            TextDecision::Binary | TextDecision::Unspecified => false,
            TextDecision::Text => self.eol != EolConversion::None,
            // `text=auto`: only when the blob does not look binary.
            TextDecision::Auto => self.eol != EolConversion::None && !looks_binary(content),
        }
    }

    /// The smudge-side LF->CRLF safety check, mirroring convert.c
    /// `will_convert_lf_to_crlf`. Returns false (no conversion) when:
    ///   * there is no naked LF to convert, or
    ///   * the action is `text=auto`-derived (the "new safer autocrlf") AND the
    ///     content already contains a lone CR or a CRLF pair, or looks binary.
    ///
    /// An explicit `text`/`eol=crlf` (non-auto) path always converts naked LFs.
    fn will_convert_lf_to_crlf(&self, content: &[u8]) -> bool {
        self.will_convert_lf_to_crlf_stats(&gather_convert_stats(content))
    }

    /// Stats-based variant of [`will_convert_lf_to_crlf`], mirroring convert.c
    /// `will_convert_lf_to_crlf(struct text_stat *, ...)`. Used by the safecrlf
    /// round-trip simulation, which mutates a copy of the stats rather than
    /// re-scanning the buffer.
    fn will_convert_lf_to_crlf_stats(&self, stats: &ConvertStats) -> bool {
        // `output_eol(crlf_action) != EOL_CRLF` short-circuits in git.
        if self.eol != EolConversion::Crlf {
            return false;
        }
        // No naked LF? Nothing to convert.
        if stats.lonelf == 0 {
            return false;
        }
        if self.text == TextDecision::Auto {
            // Any CR or CRLF already present: leave it untouched (irreversible).
            if stats.lonecr > 0 || stats.crlf > 0 {
                return false;
            }
            if convert_is_binary(stats) {
                return false;
            }
        }
        true
    }

    /// Whether this path is a candidate for the `core.safecrlf` round-trip check
    /// at all: git only warns for non-`CRLF_BINARY` actions. `Binary` and
    /// `Unspecified` (with autocrlf off) correspond to git's `CRLF_BINARY`.
    fn safecrlf_applies(&self) -> bool {
        matches!(self.text, TextDecision::Text | TextDecision::Auto)
    }

    /// Emit git's `core.safecrlf` round-trip warning for `path`, mirroring the
    /// stderr side-effect of convert.c `crlf_to_git` (the `CONV_EOL_RNDTRP_*`
    /// branch). `old_stats` are the stats of the *pre-conversion* worktree
    /// content (already gathered by the caller so the buffer is scanned once);
    /// `index_has_crlf` is whether the path's current index blob already has a
    /// CRLF (git's `has_crlf_in_index`, used only for the auto-crlf decision).
    ///
    /// This never inspects or alters the bytes written to the object store; it is
    /// purely the additive warning git prints alongside `git add`/`commit`.
    /// Returns `Err` only under `core.safecrlf=true` when the round-trip is
    /// irreversible (git `die`s).
    fn check_safe_crlf_stats(
        &self,
        old_stats: &ConvertStats,
        index_has_crlf: bool,
        flags: ConvFlags,
        path: &[u8],
    ) -> Result<()> {
        if flags == ConvFlags::Off || !self.safecrlf_applies() {
            return Ok(());
        }

        // Replicate `crlf_to_git`'s `convert_crlf_into_lf` decision (the clean
        // direction). It starts as "there is a CRLF to collapse"; auto paths
        // suppress conversion for binary content or content whose index blob
        // already carries a CRLF (the "new safer autocrlf").
        let mut convert_crlf_into_lf = old_stats.crlf > 0;
        if self.text == TextDecision::Auto {
            if convert_is_binary(old_stats) {
                // git returns 0 here: no conversion *and* no warning.
                return Ok(());
            }
            if index_has_crlf {
                convert_crlf_into_lf = false;
            }
        }

        // Simulate the round-trip on a copy of the stats.
        let mut new_stats = old_stats.clone();
        // Simulate "git add" (clean: CRLF -> LF).
        if convert_crlf_into_lf {
            new_stats.lonelf += new_stats.crlf;
            new_stats.crlf = 0;
        }
        // Simulate "git checkout" (smudge: LF -> CRLF).
        if self.will_convert_lf_to_crlf_stats(&new_stats) {
            new_stats.crlf += new_stats.lonelf;
            new_stats.lonelf = 0;
        }
        check_safe_crlf(old_stats, &new_stats, flags, path)
    }
}

/// Derive the smudge-direction line ending from `core.autocrlf` / `core.eol`.
fn eol_from_config(config: &GitConfig) -> EolConversion {
    if let Some(value) = config.get("core", None, "autocrlf") {
        match value.to_ascii_lowercase().as_str() {
            "input" => return EolConversion::Lf,
            "true" | "yes" | "on" | "1" => return EolConversion::Crlf,
            _ => {}
        }
    }
    if config.get_bool("core", None, "autocrlf") == Some(true) {
        return EolConversion::Crlf;
    }
    match config
        .get("core", None, "eol")
        .map(|v| v.to_ascii_lowercase())
    {
        Some(ref v) if v == "crlf" => EolConversion::Crlf,
        Some(ref v) if v == "lf" => EolConversion::Lf,
        _ => EolConversion::None,
    }
}

/// Whether `core.autocrlf` is set to anything that enables conversion
/// (`true` or `input`).
fn autocrlf_enabled(config: &GitConfig) -> bool {
    if let Some(value) = config.get("core", None, "autocrlf")
        && value.eq_ignore_ascii_case("input")
    {
        return true;
    }
    config.get_bool("core", None, "autocrlf") == Some(true)
}

/// Resolve the `filter=<name>` attribute against `filter.<name>.*` config.
fn resolve_filter_driver(
    config: &GitConfig,
    filter_attr: Option<&AttributeCheck>,
) -> Option<FilterDriver> {
    let name = match filter_attr.map(|check| &check.state) {
        Some(Some(AttributeState::Value(value))) => value.clone(),
        // `filter` set/unset without a value selects no driver.
        _ => return None,
    };
    let subsection = String::from_utf8_lossy(&name).into_owned();
    let clean = config
        .get("filter", Some(&subsection), "clean")
        .filter(|cmd| !cmd.is_empty())
        .map(str::to_owned);
    let smudge = config
        .get("filter", Some(&subsection), "smudge")
        .filter(|cmd| !cmd.is_empty())
        .map(str::to_owned);
    let required = config
        .get_bool("filter", Some(&subsection), "required")
        .unwrap_or(false);
    // A filter with neither command and not required is a no-op.
    if clean.is_none() && smudge.is_none() && !required {
        return None;
    }
    Some(FilterDriver {
        name,
        clean,
        smudge,
        required,
    })
}

/// Heuristic mirroring git's `buffer_is_binary`: content is treated as binary
/// when a NUL byte appears within the first 8000 bytes.
fn looks_binary(content: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    let window = &content[..content.len().min(FIRST_FEW_BYTES)];
    window.contains(&0)
}

/// Strip carriage returns that immediately precede a line feed (CRLF -> LF).
/// A lone CR (old-Mac line ending) is left untouched, matching git, which only
/// collapses CRLF pairs.
fn convert_crlf_to_lf_cow(content: Cow<'_, [u8]>) -> Cow<'_, [u8]> {
    if !content.windows(2).any(|window| window == b"\r\n") {
        return content;
    }
    let mut out = Vec::with_capacity(content.len());
    let mut index = 0;
    while index < content.len() {
        let byte = content[index];
        if byte == b'\r' && content.get(index + 1) == Some(&b'\n') {
            // Drop the CR; the LF is emitted on the next iteration.
            index += 1;
            continue;
        }
        out.push(byte);
        index += 1;
    }
    Cow::Owned(out)
}

/// Convert lone LF bytes to CRLF (LF -> CRLF). An LF already preceded by a CR
/// is left as-is so content is not double-converted, matching git.
fn convert_lf_to_crlf(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + content.len() / 16);
    let mut prev = 0u8;
    for &byte in content {
        if byte == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(byte);
        prev = byte;
    }
    out
}

/// Run a configured `clean`/`smudge` command as a subprocess, feeding `content`
/// on stdin and returning its stdout. Errors carry enough context for the
/// caller to decide whether the failure is fatal (required filter) or should be
/// silently ignored (optional filter passthrough).
fn run_filter_command(command: &str, path: &[u8], content: &[u8]) -> Result<Vec<u8>> {
    // Git expands `%f` in the filter command to the path of the file being
    // filtered (quoted). We perform the same substitution.
    let display_path = String::from_utf8_lossy(path);
    let expanded = command.replace("%f", &shell_quote(&display_path));
    // Run through the platform shell so pipelines / arguments in the configured
    // command behave the same way git's `run_command`-with-shell does.
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("/bin/sh", "-c")
    };
    let mut child = Command::new(shell)
        .arg(flag)
        .arg(&expanded)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Command(format!("failed to spawn filter `{command}`: {err}")))?;
    // Write the content to the child's stdin on a separate thread so we never
    // deadlock against a filter that streams output before consuming all input.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitError::Command(format!("filter `{command}` stdin unavailable")))?;
    let payload = content.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // Dropping `stdin` here closes the pipe so the child sees EOF.
    });
    let output = child
        .wait_with_output()
        .map_err(|err| GitError::Command(format!("filter `{command}` failed: {err}")))?;
    // Join the writer; its own errors (e.g. broken pipe) are non-fatal because
    // the child's exit status is the authoritative signal.
    let _ = writer.join();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::Command(format!(
            "filter `{command}` exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }
    Ok(output.stdout)
}

/// Minimal POSIX single-quote escaping for substituting `%f` into a shell
/// command (used only for the path passed to driver filters).
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Apply the *clean* conversion to `content` for `path` (worktree -> blob):
/// first the configured `filter.<name>.clean` driver (if any), then CRLF->LF
/// normalization when EOL conversion applies.
///
/// `config` is the repository config (`GitConfig`) and `path` is the
/// repository-relative path of the file (forward-slash separated, e.g.
/// `src/main.rs`). When no filter or EOL conversion applies the input is
/// returned unchanged.
///
/// A *required* driver (`filter.<name>.required=true`) whose `clean` command is
/// missing or fails produces a [`GitError::Command`]; a non-required driver
/// failure (or absence of a `clean` command) passes the content through
/// unfiltered, matching git.
pub fn apply_clean_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On clean the worktree file exists, so the live `.gitattributes` chain is
    // authoritative. `git_dir` is accepted for symmetry with the smudge entry
    // point (which falls back to the index) and for future use.
    let _ = git_dir.as_ref();
    let checks = filter_attribute_checks(worktree_root.as_ref(), path)?;
    apply_clean_filter_with_attributes(config, &checks, path, content)
}

/// A reusable handle that captures the worktree's `.gitattributes` chain once so
/// repeated clean-filter calls (e.g. `hash-object --stdin-paths` hashing many
/// paths in one process) don't re-walk the worktree and re-read every
/// `.gitattributes`/global config per path.
///
/// Build it once with [`WorktreeAttributes::from_worktree_root`], then call
/// [`WorktreeAttributes::apply_clean_filter`] per path. This mirrors
/// [`apply_clean_filter`] exactly except the expensive attribute-source scan is
/// amortized across calls.
pub struct WorktreeAttributes {
    matcher: AttributeMatcher,
}

impl WorktreeAttributes {
    /// Read the worktree's attribute sources once (global/`core.attributesFile`,
    /// every in-tree `.gitattributes`, and `$GIT_DIR/info/attributes`).
    pub fn from_worktree_root(worktree_root: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            matcher: AttributeMatcher::from_worktree_root(worktree_root.as_ref())?,
        })
    }

    /// Apply the clean conversion to `content` for `path`, reusing the cached
    /// attribute chain. Behaviourally identical to [`apply_clean_filter`].
    pub fn apply_clean_filter(
        &self,
        config: &GitConfig,
        path: &[u8],
        content: &[u8],
    ) -> Result<Vec<u8>> {
        let checks = self
            .matcher
            .attributes_for_path(path, &filter_attribute_names(), false);
        apply_clean_filter_with_attributes(config, &checks, path, content)
    }
}

/// A reusable handle that captures a *tree's* `.gitattributes` chain once so
/// repeated smudge-filter calls (e.g. `git archive` streaming every blob in a
/// tree) resolve attributes from the tree being processed rather than the live
/// worktree.
///
/// This is the attribute direction `git archive` uses: upstream unpacks the
/// archived tree into a scratch index and sets `GIT_ATTR_INDEX`, so the
/// `.gitattributes` that govern conversion come from the *archived tree* (plus
/// the global/`core.attributesFile` chain and `$GIT_DIR/info/attributes`), not
/// from whatever happens to be checked out. `--worktree-attributes` callers
/// should use [`WorktreeAttributes`] instead.
///
/// Build it once with [`TreeAttributes::from_tree`], then call
/// [`TreeAttributes::apply_smudge_filter`] per blob. Behaviourally this mirrors
/// [`apply_smudge_filter`] except the attribute source is the supplied tree and
/// the expensive source scan is amortized across calls.
pub struct TreeAttributes {
    matcher: AttributeMatcher,
}

impl TreeAttributes {
    /// Read the attribute sources for `tree_oid` once: the global /
    /// `core.attributesFile` chain, every `.gitattributes` blob found while
    /// walking `tree_oid`, and `$GIT_DIR/info/attributes`.
    ///
    /// `attr_root` locates the global config (`read_configured_attributes`);
    /// pass the worktree root for a non-bare repo, or the git dir for a bare
    /// one. `git_dir` locates `info/attributes` directly (so this works for bare
    /// repos, where there is no nested `.git`). No worktree `.gitattributes`
    /// files are read — use [`WorktreeAttributes`] for the
    /// `--worktree-attributes` direction.
    pub fn from_tree(
        attr_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
        db: &FileObjectDatabase,
        format: ObjectFormat,
        tree_oid: &ObjectId,
    ) -> Result<Self> {
        let attr_root = attr_root.as_ref();
        let mut matcher = AttributeMatcher::default();
        if !matcher.read_configured_attributes(attr_root) {
            matcher.read_default_global_attributes();
        }
        collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
        read_attribute_patterns(
            git_dir.as_ref().join("info").join("attributes"),
            &mut matcher,
            &[],
            b"info/attributes",
        );
        Ok(Self { matcher })
    }

    /// Apply the smudge conversion (blob -> worktree: EOL `LF`->`CRLF` plus any
    /// configured `filter.<name>.smudge` driver) to `content` for `path`,
    /// reusing the cached attribute chain. Behaviourally identical to
    /// [`apply_smudge_filter`] except attributes come from the tree this handle
    /// was built from.
    pub fn apply_smudge_filter(
        &self,
        config: &GitConfig,
        path: &[u8],
        content: &[u8],
    ) -> Result<Vec<u8>> {
        let checks = self
            .matcher
            .attributes_for_path(path, &filter_attribute_names(), false);
        apply_smudge_filter_with_attributes(config, &checks, path, content)
    }

    /// True when `path` has the `export-subst` attribute set (git's
    /// `check_attr_export_subst`), meaning `git archive` should run
    /// `$Format:…$` keyword substitution on its content.
    pub fn export_subst_for_path(&self, path: &[u8]) -> bool {
        self.attribute_is_set(path, b"export-subst")
    }

    /// True when `path` has the `export-ignore` attribute set (git's
    /// `check_attr_export_ignore`), meaning `git archive` should omit the path
    /// (and, for a directory, its whole subtree) from the archive.
    pub fn export_ignore_for_path(&self, path: &[u8]) -> bool {
        self.attribute_is_set(path, b"export-ignore")
    }

    fn attribute_is_set(&self, path: &[u8], attribute: &[u8]) -> bool {
        let requested = [attribute.to_vec()];
        let checks = self.matcher.attributes_for_path(path, &requested, false);
        matches!(
            checks.first().and_then(|check| check.state.as_ref()),
            Some(AttributeState::Set)
        )
    }

    /// The `diff` attribute state for `path` (`Set` for `diff`, `Unset` for
    /// `-diff`, `Value(name)` for `diff=<name>`, `None` when unspecified). Used
    /// by `git archive`'s zip backend to classify text vs. binary via the
    /// path's userdiff driver.
    pub fn diff_attribute_for_path(&self, path: &[u8]) -> Option<AttributeState> {
        let requested = [b"diff".to_vec()];
        let checks = self.matcher.attributes_for_path(path, &requested, false);
        checks.into_iter().next().and_then(|check| check.state)
    }
}

/// Like [`apply_clean_filter`] but takes already-resolved attribute checks,
/// letting callers that have computed attributes once reuse them.
pub fn apply_clean_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_clean_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_clean_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_clean_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    apply_clean_filter_with_attributes_cow_safecrlf(
        config,
        attributes,
        path,
        content,
        ConvFlags::Off,
        SafeCrlfIndexBlob::None,
    )
}

/// How the safecrlf check should learn whether this path's *current index blob*
/// already contains a CRLF (git's `has_crlf_in_index`). Only consulted on the
/// `text=auto` / `core.autocrlf` path.
pub enum SafeCrlfIndexBlob<'a> {
    /// No index blob is available (the staging caller has none, or safecrlf is
    /// off) — treated as "no CRLF in index".
    None,
    /// The path's current index blob, read on demand from this object database
    /// only when the auto-crlf decision actually needs it.
    Lookup {
        odb: &'a FileObjectDatabase,
        oid: ObjectId,
    },
}

impl SafeCrlfIndexBlob<'_> {
    fn has_crlf(&self) -> bool {
        match self {
            SafeCrlfIndexBlob::None => false,
            SafeCrlfIndexBlob::Lookup { odb, oid } => has_crlf_in_index(odb, oid),
        }
    }
}

/// [`apply_clean_filter_with_attributes_cow`] plus git's additive `core.safecrlf`
/// round-trip warning (convert.c `crlf_to_git`).
///
/// The conversion result is byte-for-byte identical to the plain variant;
/// `flags`/`index_blob` only drive the stderr warning git prints when a
/// CRLF<->LF round-trip would not be reversible. The warning is computed on the
/// *post-driver, pre-EOL-conversion* content, matching git's ordering in
/// `convert_to_git` (apply_filter -> crlf_to_git).
pub fn apply_clean_filter_with_attributes_cow_safecrlf<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
    flags: ConvFlags,
    index_blob: SafeCrlfIndexBlob<'_>,
) -> Result<Cow<'a, [u8]>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    let mut data = Cow::Borrowed(content);
    if let Some(driver) = &plan.driver {
        data = run_driver(driver, driver.clean.as_deref(), path, data)?;
    }
    // The safecrlf check scans the (post-driver) buffer once for line-ending
    // stats. Gate it tightly so the extra scan never runs on the dominant
    // pass-through paths: only when safecrlf is enabled, the path is a real
    // conversion candidate (not `CRLF_BINARY`), and the buffer is non-empty.
    if flags != ConvFlags::Off && !data.is_empty() && plan.safecrlf_applies() {
        let old_stats = gather_convert_stats(&data);
        plan.check_safe_crlf_stats(&old_stats, index_blob.has_crlf(), flags, path)?;
    }
    if plan.convert_eol(&data) {
        data = convert_crlf_to_lf_cow(data);
    }
    Ok(data)
}

/// Apply the *smudge* conversion to `content` for `path` (blob -> worktree):
/// first LF->CRLF when EOL conversion applies, then the configured
/// `filter.<name>.smudge` driver (if any).
///
/// Semantics mirror [`apply_clean_filter`]: a required driver with a missing or
/// failing `smudge` command errors, while a non-required one passes the content
/// through.
pub fn apply_smudge_filter(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    config: &GitConfig,
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    // On smudge (checkout) the worktree file may not exist yet, so resolve the
    // attributes from the `.gitattributes` recorded in the index.
    let checks =
        smudge_attribute_checks_from_index(worktree_root.as_ref(), git_dir.as_ref(), format, path)?;
    apply_smudge_filter_with_attributes(config, &checks, path, content)
}

/// Like [`apply_smudge_filter`] but takes already-resolved attribute checks.
pub fn apply_smudge_filter_with_attributes(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &[u8],
) -> Result<Vec<u8>> {
    Ok(apply_smudge_filter_with_attributes_cow(config, attributes, path, content)?.into_owned())
}

/// Borrow-first variant of [`apply_smudge_filter_with_attributes`].
///
/// When no filter or EOL conversion changes the content, the returned value
/// borrows `content`; callers that can consume a [`Cow`] avoid allocating for
/// the common pass-through case.
pub fn apply_smudge_filter_with_attributes_cow<'a>(
    config: &GitConfig,
    attributes: &[AttributeCheck],
    path: &[u8],
    content: &'a [u8],
) -> Result<Cow<'a, [u8]>> {
    let plan = ContentFilterPlan::resolve(config, attributes);
    let mut data = Cow::Borrowed(content);
    if plan.eol == EolConversion::Crlf
        && plan.convert_eol(&data)
        && plan.will_convert_lf_to_crlf(&data)
    {
        data = Cow::Owned(convert_lf_to_crlf(&data));
    }
    if let Some(driver) = &plan.driver {
        data = run_driver(driver, driver.smudge.as_deref(), path, data)?;
    }
    Ok(data)
}

/// Execute one direction of a driver filter, honouring the `required` flag.
fn run_driver<'a>(
    driver: &FilterDriver,
    command: Option<&str>,
    path: &[u8],
    content: Cow<'a, [u8]>,
) -> Result<Cow<'a, [u8]>> {
    let Some(command) = command else {
        // No command in this direction. Required filters must error; optional
        // ones pass content through unchanged.
        if driver.required {
            return Err(GitError::Command(format!(
                "required filter `{}` has no configured command for this direction",
                String::from_utf8_lossy(&driver.name)
            )));
        }
        return Ok(content);
    };
    match run_filter_command(command, path, &content) {
        Ok(output) => Ok(Cow::Owned(output)),
        Err(err) => {
            if driver.required {
                Err(err)
            } else {
                // Non-required filter failure: fall back to the unfiltered
                // content, matching git's behaviour.
                Ok(content)
            }
        }
    }
}

/// Compute the attributes relevant to content filtering (`text`, `eol`,
/// `filter`) for `path` from the worktree `.gitattributes` chain.
fn filter_attribute_checks(worktree_root: &Path, path: &[u8]) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    let mut matcher = AttributeMatcher::default();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    read_dir_attribute_patterns_for_base(worktree_root, &[], &mut matcher)?;
    let mut prefix = Vec::new();
    let mut parts = path.split(|byte| *byte == b'/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(part);
        let dir = worktree_root.join(repo_path_to_os_path(&prefix)?);
        read_dir_attribute_patterns_for_base(&dir, &prefix, &mut matcher)?;
    }
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher.attributes_for_path(path, &requested, false))
}

/// Compute filtering attributes for a checkout (blob -> worktree).
///
/// `git checkout -- <pathspec>` / `git restore` materialize through git's
/// **default** attr direction, which is `GIT_ATTR_CHECKIN` (attr.c: the static
/// `direction` is zero-initialized and `builtin/checkout.c` never overrides it
/// for the pathspec path). Under that direction `read_attr` reads each
/// `.gitattributes` frame from the **worktree file first**, falling back to the
/// staged blob only when no worktree file exists at that directory level
/// (sparse-checkout). This is the precedence the smudge filter must use:
/// t0027 commits an *empty* root `.gitattributes`, then overwrites the worktree
/// copy with `*.txt text eol=crlf` *without re-staging* — and git's checkout
/// still honours the worktree copy. Reading the index alone (or index-first)
/// made checkout under-convert line endings, because the staged blob was empty.
fn smudge_attribute_checks_from_index(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    let requested = filter_attribute_names();
    let mut matcher = AttributeMatcher::default();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }

    // Build the set of `.gitattributes` blobs the index carries, keyed by the
    // directory they govern, so each ancestry frame can prefer the staged copy.
    let index_attributes = index_gitattributes_by_base(git_dir, format)?;

    // Walk root -> ... -> the file's parent directory, folding each frame's
    // `.gitattributes` in shallow-to-deep order so deeper directories win.
    fold_checkout_attribute_frame(worktree_root, &[], &index_attributes, &mut matcher)?;
    let mut prefix = Vec::new();
    let mut parts = path.split(|byte| *byte == b'/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        prefix.extend_from_slice(part);
        let dir = worktree_root.join(repo_path_to_os_path(&prefix)?);
        fold_checkout_attribute_frame(&dir, &prefix, &index_attributes, &mut matcher)?;
    }

    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher.attributes_for_path(path, &requested, false))
}

/// Fold the `.gitattributes` governing directory `base` (whose on-disk location
/// is `dir`) into `matcher`, preferring the worktree file and falling back to
/// the staged blob. Mirrors one attr-stack frame under `GIT_ATTR_CHECKIN`
/// (git's default direction, used by `checkout -- <pathspec>` / `restore`).
fn fold_checkout_attribute_frame(
    dir: &Path,
    base: &[u8],
    index_attributes: &BTreeMap<Vec<u8>, Vec<u8>>,
    matcher: &mut AttributeMatcher,
) -> Result<()> {
    let worktree_file = dir.join(".gitattributes");
    if let Ok(contents) = fs::read(&worktree_file) {
        // A worktree `.gitattributes` exists at this level: it wins outright
        // (git only consults the index when the worktree file is absent).
        read_attribute_patterns_from_bytes(&contents, matcher, base);
    } else if let Some(contents) = index_attributes.get(base) {
        read_attribute_patterns_from_bytes(contents, matcher, base);
    }
    Ok(())
}

/// Read every staged `.gitattributes` blob, keyed by the repo-relative directory
/// it governs (`""` for the worktree root). Stage-0 blob entries only.
fn index_gitattributes_by_base(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut map = BTreeMap::new();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(map);
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let entries = Index::parse(&fs::read(index_path)?, format)?.entries;
    for entry in entries {
        let is_attributes_file =
            entry.path == b".gitattributes" || entry.path.as_bytes().ends_with(b"/.gitattributes");
        if index_entry_stage(&entry) != 0
            || tree_entry_object_type(entry.mode) != ObjectType::Blob
            || !is_attributes_file
        {
            continue;
        }
        let base = match entry.path.as_bytes().strip_suffix(b".gitattributes") {
            Some(b"") => Vec::new(),
            Some(parent) => parent.strip_suffix(b"/").unwrap_or(parent).to_vec(),
            None => continue,
        };
        let object = db
            .read_object(&entry.oid)
            .map_err(|err| expect_missing_object_kind(err, entry.oid, MissingObjectKind::Blob))?;
        if object.object_type == ObjectType::Blob {
            map.insert(base, object.body.clone());
        }
    }
    Ok(map)
}

fn filter_attribute_names() -> Vec<Vec<u8>> {
    // `crlf` is git's legacy alias for `text` (convert.c registers both); it is
    // consulted as a fallback when `text` is unspecified, so we must resolve it.
    vec![
        b"text".to_vec(),
        b"crlf".to_vec(),
        b"eol".to_vec(),
        b"filter".to_vec(),
    ]
}

// ---------------------------------------------------------------------------
// `ls-files --eol` line-ending information
//
// Git's `git ls-files --eol` prints, for each path, three fields:
//   i/<stat>  — line-ending statistics of the *index* blob content
//   w/<stat>  — line-ending statistics of the *worktree* file content
//   attr/<a>  — the resolved crlf/eol attribute action (attributes only, no
//               config) — `get_convert_attr_ascii` in convert.c
// The two stat fields mirror `gather_convert_stats_ascii`; the attr field
// mirrors `convert_attrs` up to `ca->attr_action` (i.e. *before* the config
// derived `text` -> input/crlf substitution and the `core.autocrlf` fallback).
// ---------------------------------------------------------------------------

/// Line-ending statistics of a byte buffer, mirroring convert.c `gather_stats`.
#[derive(Clone)]
struct ConvertStats {
    nul: u32,
    lonecr: u32,
    lonelf: u32,
    crlf: u32,
    printable: u32,
    nonprintable: u32,
}

fn gather_convert_stats(buf: &[u8]) -> ConvertStats {
    let mut stats = ConvertStats {
        nul: 0,
        lonecr: 0,
        lonelf: 0,
        crlf: 0,
        printable: 0,
        nonprintable: 0,
    };
    let mut i = 0;
    while i < buf.len() {
        let c = buf[i];
        if c == b'\r' {
            if buf.get(i + 1) == Some(&b'\n') {
                stats.crlf += 1;
                i += 1;
            } else {
                stats.lonecr += 1;
            }
            i += 1;
            continue;
        }
        if c == b'\n' {
            stats.lonelf += 1;
            i += 1;
            continue;
        }
        if c == 127 {
            // DEL
            stats.nonprintable += 1;
        } else if c < 32 {
            match c {
                // BS, HT, ESC and FF are printable.
                0x08 | 0x09 | 0x1b | 0x0c => stats.printable += 1,
                0 => {
                    stats.nul += 1;
                    stats.nonprintable += 1;
                }
                _ => stats.nonprintable += 1,
            }
        } else {
            stats.printable += 1;
        }
        i += 1;
    }
    // A trailing EOF (^Z, 0x1a) is not counted as non-printable.
    if buf.last() == Some(&0x1a) {
        stats.nonprintable = stats.nonprintable.saturating_sub(1);
    }
    stats
}

/// Mirror of convert.c `has_crlf_in_index`: whether the blob currently recorded
/// in the index for this path is non-binary text containing a CRLF. Used only by
/// the auto-crlf safecrlf decision to keep an already-CRLF index blob from being
/// silently collapsed. A missing/unreadable blob (or a non-blob entry) counts as
/// "no CRLF", matching git's `read_blob_data_from_index` returning NULL.
fn has_crlf_in_index(odb: &FileObjectDatabase, oid: &ObjectId) -> bool {
    let Ok(object) = odb.read_object(oid) else {
        return false;
    };
    if object.object_type != ObjectType::Blob {
        return false;
    }
    let data = &object.body;
    // git short-circuits on the first '\r' via memchr before gathering stats.
    if !data.contains(&b'\r') {
        return false;
    }
    let stats = gather_convert_stats(data);
    !convert_is_binary(&stats) && stats.crlf > 0
}

/// Mirror of convert.c `convert_is_binary`: a lone CR or NUL, or a high
/// non-printable ratio, marks the content as binary.
fn convert_is_binary(stats: &ConvertStats) -> bool {
    if stats.lonecr > 0 {
        return true;
    }
    if stats.nul > 0 {
        return true;
    }
    (stats.printable >> 7) < stats.nonprintable
}

/// The `core.safecrlf` round-trip-warning mode, mirroring git's
/// `global_conv_flags_eol` (environment.c). git's *default* — when
/// `core.safecrlf` is unset — is [`ConvFlags::Warn`], so the warning fires even
/// without any explicit config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvFlags {
    /// `core.safecrlf=false`: never warn.
    Off,
    /// `core.safecrlf=warn` (and the unset default): emit a warning when a
    /// CRLF<->LF round-trip would not be reversible.
    Warn,
    /// `core.safecrlf=true`: die instead of warn.
    Die,
}

impl ConvFlags {
    /// Resolve `core.safecrlf` from config, mirroring environment.c
    /// `git_default_core_config`: `warn` -> [`ConvFlags::Warn`], a boolean-true
    /// value -> [`ConvFlags::Die`], a boolean-false value -> [`ConvFlags::Off`].
    /// When the key is absent git leaves `global_conv_flags_eol` at its initial
    /// [`ConvFlags::Warn`], so unset also resolves to [`ConvFlags::Warn`].
    pub fn from_config(config: &GitConfig) -> Self {
        match config.get("core", None, "safecrlf") {
            Some(value) if value.eq_ignore_ascii_case("warn") => ConvFlags::Warn,
            Some(_) => {
                if config.get_bool("core", None, "safecrlf") == Some(true) {
                    ConvFlags::Die
                } else {
                    ConvFlags::Off
                }
            }
            None => ConvFlags::Warn,
        }
    }
}

/// Mirror of convert.c `check_global_conv_flags_eol`: compare the pre-conversion
/// `old_stats` against the simulated round-trip `new_stats` and, when the
/// CRLF/LF content would not survive a clean+smudge cycle, warn (or die under
/// `core.safecrlf=true`).
///
/// Returns `Err(GitError::Exit(128))` when `flags` is [`ConvFlags::Die`] and the
/// round-trip is irreversible (git `die`s with exit 128 here); otherwise prints
/// the warning to stderr and returns `Ok(())`. This is a pure stderr-side
/// effect: it never changes the bytes written to the object store.
fn check_safe_crlf(
    old_stats: &ConvertStats,
    new_stats: &ConvertStats,
    flags: ConvFlags,
    path: &[u8],
) -> Result<()> {
    if flags == ConvFlags::Off {
        return Ok(());
    }
    let display = String::from_utf8_lossy(path);
    if old_stats.crlf > 0 && new_stats.crlf == 0 {
        // CRLFs would not be restored by checkout.
        match flags {
            ConvFlags::Die => {
                eprintln!("fatal: CRLF would be replaced by LF in {display}");
                return Err(GitError::Exit(128));
            }
            ConvFlags::Warn => {
                eprintln!(
                    "warning: in the working copy of '{display}', CRLF will be replaced by LF the next time Git touches it"
                );
            }
            ConvFlags::Off => unreachable!("handled above"),
        }
    } else if old_stats.lonelf > 0 && new_stats.lonelf == 0 {
        // CRLFs would be added by checkout.
        match flags {
            ConvFlags::Die => {
                eprintln!("fatal: LF would be replaced by CRLF in {display}");
                return Err(GitError::Exit(128));
            }
            ConvFlags::Warn => {
                eprintln!(
                    "warning: in the working copy of '{display}', LF will be replaced by CRLF the next time Git touches it"
                );
            }
            ConvFlags::Off => unreachable!("handled above"),
        }
    }
    Ok(())
}

/// Compute the `i/` or `w/` stat string for `content`, mirroring
/// convert.c `gather_convert_stats_ascii`.
fn convert_stats_ascii(content: &[u8]) -> &'static str {
    if content.is_empty() {
        return "none";
    }
    let stats = gather_convert_stats(content);
    if convert_is_binary(&stats) {
        return "-text";
    }
    match (stats.lonelf > 0, stats.crlf > 0) {
        (true, false) => "lf",
        (false, true) => "crlf",
        (true, true) => "mixed",
        (false, false) => "none",
    }
}

/// The resolved crlf/eol attribute action for a path, mirroring convert.c
/// `convert_attrs` up to `ca->attr_action` (attributes only, no config), and
/// `get_convert_attr_ascii` for the ascii spelling.
fn convert_attr_ascii(checks: &[AttributeCheck]) -> &'static str {
    fn state_of<'a>(checks: &'a [AttributeCheck], name: &[u8]) -> Option<&'a AttributeState> {
        checks
            .iter()
            .find(|check| check.attribute == name)
            .and_then(|check| check.state.as_ref())
    }

    // git_path_check_crlf: ATTR_TRUE -> TEXT, ATTR_FALSE -> BINARY,
    // ATTR_UNSET -> (fall through), "input" -> TEXT_INPUT, "auto" -> AUTO,
    // anything else -> UNDEFINED.
    #[derive(Clone, Copy, PartialEq)]
    enum Action {
        Undefined,
        Binary,
        Text,
        TextInput,
        TextCrlf,
        Auto,
        AutoCrlf,
        AutoInput,
    }
    fn check_crlf(state: Option<&AttributeState>) -> Action {
        match state {
            Some(AttributeState::Set) => Action::Text,
            Some(AttributeState::Unset) => Action::Binary,
            Some(AttributeState::Value(value)) if value == b"input" => Action::TextInput,
            Some(AttributeState::Value(value)) if value == b"auto" => Action::Auto,
            // ATTR_UNSET / any other value -> CRLF_UNDEFINED.
            _ => Action::Undefined,
        }
    }

    // Resolve from the `text` attribute, then fall back to the legacy `crlf`
    // alias only when `text` left the action undefined.
    let mut action = check_crlf(state_of(checks, b"text"));
    if action == Action::Undefined {
        action = check_crlf(state_of(checks, b"crlf"));
    }

    if action != Action::Binary {
        // git_path_check_eol: only "lf"/"crlf" values matter.
        let eol = match state_of(checks, b"eol") {
            Some(AttributeState::Value(value)) if value == b"lf" => Some(false),
            Some(AttributeState::Value(value)) if value == b"crlf" => Some(true),
            _ => None,
        };
        action = match (action, eol) {
            (Action::Auto, Some(false)) => Action::AutoInput,
            (Action::Auto, Some(true)) => Action::AutoCrlf,
            (_, Some(false)) if action != Action::Auto => Action::TextInput,
            (_, Some(true)) if action != Action::Auto => Action::TextCrlf,
            _ => action,
        };
    }

    match action {
        Action::Undefined => "",
        Action::Binary => "-text",
        Action::Text => "text",
        Action::TextInput => "text eol=lf",
        Action::TextCrlf => "text eol=crlf",
        Action::Auto => "text=auto",
        Action::AutoCrlf => "text=auto eol=crlf",
        Action::AutoInput => "text=auto eol=lf",
    }
}

/// The three `ls-files --eol` fields for a single path.
pub struct EolInfo {
    /// Stat of the index blob (`i/...`); empty when there is no index blob.
    pub index: &'static str,
    /// Stat of the worktree file (`w/...`); empty when the file is absent.
    pub worktree: &'static str,
    /// Resolved crlf/eol attribute action (`attr/...`).
    pub attr: &'static str,
}

impl EolInfo {
    /// Format as git's `ls-files --eol` prefix: `i/%-5s w/%-5s attr/%-17s\t`.
    pub fn format_prefix(&self) -> String {
        format!(
            "i/{:<5} w/{:<5} attr/{:<17}\t",
            self.index, self.worktree, self.attr
        )
    }
}

/// Compute the `ls-files --eol` info for `path`.
///
/// `index_content` is the raw index blob bytes (None when the path has no
/// index entry or is not a regular file). The worktree file is read from
/// `worktree_root/path`; if it is absent or not a regular file the `w/` field
/// is empty. Attributes are resolved from the worktree `.gitattributes` chain
/// via `attr_checks`.
pub fn eol_info_for_path(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
    index_content: Option<&[u8]>,
    attr_checks: &[AttributeCheck],
) -> EolInfo {
    let index = index_content.map(convert_stats_ascii).unwrap_or("");

    let worktree_root = worktree_root.as_ref();
    let worktree = match repo_path_to_os_path(path) {
        Ok(rel) => {
            let absolute = worktree_root.join(rel);
            match fs::symlink_metadata(&absolute) {
                // git: only regular files get a `w/` stat (lstat + S_ISREG).
                Ok(meta) if meta.file_type().is_file() => match fs::read(&absolute) {
                    Ok(content) => convert_stats_ascii_owned(&content),
                    Err(_) => "",
                },
                _ => "",
            }
        }
        Err(_) => "",
    };

    let attr = convert_attr_ascii(attr_checks);

    EolInfo {
        index,
        worktree,
        attr,
    }
}

/// `convert_stats_ascii` over an owned buffer; the result is a `'static` str so
/// the buffer can be dropped.
fn convert_stats_ascii_owned(content: &[u8]) -> &'static str {
    convert_stats_ascii(content)
}

/// Resolve the crlf/eol/text/filter attributes for `path` from the worktree
/// `.gitattributes` chain (the set `ls-files --eol` needs for its `attr/`
/// field).
pub fn eol_attribute_checks(
    worktree_root: impl AsRef<Path>,
    path: &[u8],
) -> Result<Vec<AttributeCheck>> {
    filter_attribute_checks(worktree_root.as_ref(), path)
}

pub fn deleted_index_entries(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<IndexEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let mut deleted = Vec::new();
    for entry in index.entries {
        if !worktree_path(worktree_root, entry.path.as_bytes())?.exists() {
            deleted.push(entry);
        }
    }
    Ok(deleted)
}

pub fn modified_index_entries(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
) -> Result<Vec<IndexEntry>> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    // Reuse the same racy-git stat shortcut here: build the cache from the index
    // we just parsed (no second parse) so the worktree walk can skip re-hashing
    // unchanged files. A cached oid is only trusted on a non-racy stat match, so
    // genuinely modified files still fall through to a hash and are reported.
    let stat_cache = IndexStatCache::from_index(&index, &index_path);
    let worktree = worktree_entries_with_stat_cache(
        worktree_root,
        git_dir,
        format,
        Some(&stat_cache),
        None,
        None,
    )?;
    let mut modified = Vec::new();
    for entry in index.entries {
        let Some(worktree_entry) = worktree.get(entry.path.as_bytes()) else {
            modified.push(entry);
            continue;
        };
        if worktree_entry.mode != entry.mode || worktree_entry.oid != entry.oid {
            modified.push(entry);
        }
    }
    Ok(modified)
}

pub fn checkout_branch(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    branch: &str,
    committer: Vec<u8>,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let branch_ref = branch_ref_name(branch)?;
    let refs = FileRefStore::new(git_dir, format);
    let target = match sley_refs::resolve_ref_peeled(&refs, &branch_ref)? {
        Some(oid) => oid,
        None => {
            checkout_switch_head_symbolic(&refs, branch_ref, committer, branch, None, None)?;
            return Ok(CheckoutResult {
                branch: branch.into(),
                oid: ObjectId::null(format),
                files: 0,
            });
        }
    };
    let current_head = resolve_head_commit_oid(git_dir, format)?;
    let files = if current_head == Some(target) {
        0
    } else {
        checkout_commit_to_index_and_worktree(worktree_root, git_dir, format, &target)?
    };
    checkout_switch_head_symbolic(
        &refs,
        branch_ref,
        committer,
        branch,
        Some(target),
        Some(target),
    )?;
    Ok(CheckoutResult {
        branch: branch.into(),
        oid: target,
        files,
    })
}

pub fn checkout_detached(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree(worktree_root, git_dir, format, target)?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

/// Like [`checkout_branch`], but runs the smudge-side content filters
/// (`core.autocrlf`/`text`/`eol` EOL conversion and `filter.<name>.smudge`
/// drivers) on each blob as it is written to the worktree. `config` is the
/// repository config used to resolve the filters.
pub fn checkout_branch_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    branch: &str,
    committer: Vec<u8>,
    config: &GitConfig,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let branch_ref = branch_ref_name(branch)?;
    let refs = FileRefStore::new(git_dir, format);
    let target = match sley_refs::resolve_ref_peeled(&refs, &branch_ref)? {
        Some(oid) => oid,
        None => {
            checkout_switch_head_symbolic(&refs, branch_ref, committer, branch, None, None)?;
            return Ok(CheckoutResult {
                branch: branch.into(),
                oid: ObjectId::null(format),
                files: 0,
            });
        }
    };
    let current_head = resolve_head_commit_oid(git_dir, format)?;
    let files = if current_head == Some(target) {
        0
    } else {
        checkout_commit_to_index_and_worktree_filtered(
            worktree_root,
            git_dir,
            format,
            &target,
            Some(config),
        )?
    };
    checkout_switch_head_symbolic(
        &refs,
        branch_ref,
        committer,
        branch,
        Some(target),
        Some(target),
    )?;
    Ok(CheckoutResult {
        branch: branch.into(),
        oid: target,
        files,
    })
}

/// Like [`checkout_detached`], but runs the smudge-side content filters (see
/// [`checkout_branch_filtered`]).
pub fn checkout_detached_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
    config: &GitConfig,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree_filtered(
        worktree_root,
        git_dir,
        format,
        target,
        Some(config),
    )?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

fn checkout_commit_to_index_and_worktree(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
) -> Result<usize> {
    checkout_commit_to_index_and_worktree_filtered(worktree_root, git_dir, format, target, None)
}

/// Like [`checkout_commit_to_index_and_worktree`] but optionally runs the
/// smudge-side content filters (see [`apply_smudge_filter`]) on each blob before
/// it is written to the worktree. Attribute lookups use the `.gitattributes`
/// recorded in the *target tree* so the rules of the checked-out commit apply.
fn checkout_commit_to_index_and_worktree_filtered(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    smudge_config: Option<&GitConfig>,
) -> Result<usize> {
    let mut dirty = false;
    stream_short_status(worktree_root, git_dir, format, |entry| {
        if !status_row_is_untracked_or_ignored(entry) {
            dirty = true;
            return Ok(StreamControl::Stop);
        }
        Ok(StreamControl::Continue)
    })?;
    if dirty {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    let attributes = smudge_config
        .map(|_| build_tree_attribute_matcher(worktree_root, &db, format, &commit.tree))
        .transpose()?;

    for path in read_index_entries(git_dir, format)?.keys() {
        if !target_entries.contains_key(path) {
            remove_worktree_file(worktree_root, path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        // Gitlinks go through the shared materialization step (mkdir + zeroed
        // stat); smudge filters never apply to a submodule directory.
        if sley_index::is_gitlink(entry.mode) {
            index_entries.push(materialize_tree_entry(&db, worktree_root, path, entry)?);
            continue;
        }
        let object = read_expected_object(&db, &entry.oid, ObjectType::Blob)?;
        let body: Cow<'_, [u8]> = match (smudge_config, &attributes) {
            (Some(config), Some(matcher)) => {
                let checks = matcher.attributes_for_path(path, &filter_attribute_names(), false);
                apply_smudge_filter_with_attributes_cow(config, &checks, path, &object.body)?
            }
            _ => Cow::Borrowed(&object.body),
        };
        let file_path = worktree_path(worktree_root, path)?;
        prepare_blob_parent_dirs(worktree_root, &file_path)?;
        remove_existing_worktree_path(&file_path)?;
        fs::write(&file_path, &body)?;
        set_worktree_file_mode(&file_path, entry.mode)?;
        let metadata = fs::metadata(&file_path)?;
        let mut index_entry = index_entry_from_metadata(path.clone(), entry.oid, &metadata);
        index_entry.mode = entry.mode;
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(target_entries.len())
}

/// Build an [`AttributeMatcher`] from the `.gitattributes` files contained in a
/// tree, plus the repo-level (`core.attributesFile`, `.git/info/attributes`)
/// sources, mirroring [`standard_attributes_for_path_from_tree`].
fn build_tree_attribute_matcher(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<AttributeMatcher> {
    let mut matcher = AttributeMatcher::default();
    if !matcher.read_configured_attributes(worktree_root) {
        matcher.read_default_global_attributes();
    }
    collect_attribute_patterns_from_tree(db, format, tree_oid, Vec::new(), &mut matcher)?;
    read_attribute_patterns(
        worktree_root.join(".git").join("info").join("attributes"),
        &mut matcher,
        &[],
        b".git/info/attributes",
    );
    Ok(matcher)
}

/// Sparse- and skip-worktree-aware variant of
/// [`checkout_commit_to_index_and_worktree`].
///
/// When `sparse` is `None` this behaves like the plain checkout except that it
/// preserves any pre-existing skip-worktree bits (so an already-sparse worktree
/// is not silently re-expanded). When `sparse` is `Some`, every target path is
/// additionally classified against the patterns: in-cone paths are written and
/// have their skip-worktree bit cleared, while out-of-cone paths are left out
/// of the worktree, get their skip-worktree bit set, and have any stale file
/// removed.
fn checkout_commit_to_index_and_worktree_sparse(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    sparse: Option<(&SparseCheckout, SparseCheckoutMode)>,
) -> Result<usize> {
    let previously_skipped = skip_worktree_paths(git_dir, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, target)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    // Honor skip-worktree: a path whose worktree file is intentionally absent
    // must not be treated as a dirty (deleted) change blocking the checkout.
    let mut dirty = false;
    stream_short_status(worktree_root, git_dir, format, |entry| {
        if previously_skipped.contains(entry.path) {
            return Ok(StreamControl::Continue);
        }
        // Submodule state never blocks a checkout: upstream unpack-trees
        // treats gitlinks as always up-to-date (ie_match_stat refuses to pay
        // for a submodule dirtiness probe), so new commits / dirty content in
        // a submodule must not fail the branch switch.
        if entry.index_mode.is_some_and(sley_index::is_gitlink)
            || entry.worktree_mode.is_some_and(sley_index::is_gitlink)
        {
            return Ok(StreamControl::Continue);
        }
        // An untracked embedded repository where the target tree records a
        // gitlink is reused as-is (upstream entry.c write_entry: mkdir with
        // EEXIST is success), so it does not block the checkout either.
        if entry.index == b'?' && entry.worktree == b'?' {
            let path = entry
                .path
                .strip_suffix(b"/")
                .unwrap_or(entry.path);
            if target_entries
                .get(path)
                .is_some_and(|target| sley_index::is_gitlink(target.mode))
            {
                return Ok(StreamControl::Continue);
            }
        }
        dirty = true;
        Ok(StreamControl::Stop)
    })?;
    if dirty {
        return Err(GitError::Transaction(
            "checkout requires a clean working tree".into(),
        ));
    }

    let matcher = sparse.map(|(spec, mode)| SparseMatcher::new(spec, mode));

    for path in read_index_entries(git_dir, format)?.keys() {
        if target_entries.contains_key(path) {
            continue;
        }
        // Do not disturb the worktree state of an intentionally skipped path.
        if previously_skipped.contains(path) {
            continue;
        }
        remove_worktree_file(worktree_root, path)?;
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let in_cone = matcher.as_ref().is_none_or(|matcher| {
            // A path already marked skip-worktree stays out unless it now
            // matches the sparse cone, mirroring upstream "honor skip-worktree".
            matcher.includes_file(path)
        });
        let index_entry = if in_cone {
            // `materialize_tree_entry` leaves flags_extended at 0, so the
            // skip-worktree bit is already clear for in-cone paths.
            materialize_tree_entry(&db, worktree_root, path, entry)?
        } else {
            // Out of cone: ensure no stale worktree file remains and synthesize
            // an index entry straight from the tree (no worktree metadata),
            // then mark it skip-worktree.
            remove_worktree_file(worktree_root, path)?;
            let mut index_entry = restored_head_index_entry(worktree_root, &db, path, entry)?;
            set_skip_worktree(&mut index_entry);
            index_entry
        };
        index_entries.push(index_entry);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions: Vec::new(),
        checksum: None,
    };
    normalize_index_version_for_extended_flags(&mut index);
    fs::write(repository_index_path(git_dir), index.write(format)?)?;
    Ok(target_entries.len())
}

fn skip_worktree_paths(git_dir: &Path, format: ObjectFormat) -> Result<BTreeSet<Vec<u8>>> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Ok(BTreeSet::new());
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    Ok(index
        .entries
        .into_iter()
        .filter(index_entry_skip_worktree)
        .map(|entry| entry.path.into_bytes())
        .collect())
}

pub fn restore_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    restore_worktree_paths_inner(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        None,
    )
}

/// Like [`restore_worktree_paths`], applying the smudge-side content filters
/// (CRLF / ident / filter drivers) the way a checkout writes blobs.
pub fn restore_worktree_paths_filtered(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    config: &GitConfig,
) -> Result<RestoreResult> {
    restore_worktree_paths_inner(
        worktree_root.as_ref(),
        git_dir.as_ref(),
        format,
        paths,
        Some(config),
    )
}

fn restore_worktree_paths_inner(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    paths: &[PathBuf],
    smudge_config: Option<&GitConfig>,
) -> Result<RestoreResult> {
    let index_path = repository_index_path(git_dir);
    if !index_path.exists() {
        return Err(GitError::Exit(1));
    }
    let index = Index::parse(&fs::read(index_path)?, format)?;
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_has_entry_under(&index.entries, &git_path);
        let mut matched = false;
        for entry in &index.entries {
            if entry.path.as_bytes() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(entry.path.as_bytes(), &git_path))
            {
                restore_index_entry(worktree_root, git_dir, format, &db, entry, smudge_config)?;
                restored.insert(entry.path.clone());
                matched = true;
            }
        }
        if !matched {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
    }
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn restore_index_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        paths,
    )
}

pub fn restore_index_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_index_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        paths,
    )
}

fn restore_index_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.keys().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                // git's pathspec reset (`reset_index` → diff against the source
                // tree) only rewrites entries that actually CHANGE: an entry whose
                // oid and mode already equal the source is left untouched, so its
                // cached stat is preserved and `git diff-files` stays clean (t7102
                // "resetting an unmodified path is a no-op"). Only when the entry
                // genuinely changes does git write a fresh, stat-zeroed entry.
                let unchanged = index_entries.get(&path).is_some_and(|existing| {
                    existing.oid == entry.oid && existing.mode == entry.mode
                });
                if !unchanged {
                    index_entries.insert(
                        path.clone(),
                        restored_head_index_entry(worktree_root, db, &path, entry)?,
                    );
                }
            } else {
                index_entries.remove(&path);
            }
            restored.insert(path);
        }
    }
    let mut entries = index_entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn restore_index_and_worktree_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_index_and_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &head_entries,
        paths,
    )
}

pub fn restore_index_and_worktree_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_index_and_worktree_paths_from_entries(
        worktree_root,
        git_dir,
        format,
        &db,
        index,
        &source_entries,
        paths,
    )
}

fn restore_index_and_worktree_paths_from_entries(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let mut index_entries = index
        .entries
        .into_iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.keys().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                index_entries.insert(
                    path.clone(),
                    restore_head_entry_to_worktree_and_index(worktree_root, db, &path, entry)?,
                );
            } else {
                index_entries.remove(&path);
                remove_worktree_file(worktree_root, &path)?;
            }
            restored.insert(path);
        }
    }
    let mut entries = index_entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn reset_index_and_worktree_to_commit(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, commit_oid)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;

    // git's `reset --hard` runs a one-way merge through unpack-trees: EVERY path
    // present in the current index (at ANY stage) that the target tree does not
    // track is removed from the worktree. A conflicted D/F merge can leave a
    // path like `dir~HEAD` at stage 2 only — those entries are dropped by the
    // stage-0-only `read_index_entries`, so iterate the RAW index paths here
    // (deduped across stages) to match git and delete the moved-aside file.
    for path in current_index_paths(git_dir, format, &db)? {
        if !target_entries.contains_key(&path) {
            remove_worktree_file(worktree_root, &path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        index_entries.push(materialize_tree_entry(&db, worktree_root, path, entry)?);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

/// All paths the current index references, deduped across stages (a conflicted
/// path appears at stages 1–3; we want it listed once). Unlike
/// `read_index_entries`, which filters to stage 0, this keeps conflicted paths
/// so a `reset --hard` worktree sweep removes moved-aside files (`dir~HEAD`) the
/// target tree doesn't track — matching git's one-way unpack-trees behavior.
fn current_index_paths(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<BTreeSet<Vec<u8>>> {
    let (index, _stat_cache, _head_matches) = read_index_with_stat_cache(git_dir, format, db)?;
    Ok(index
        .entries
        .into_iter()
        .map(|entry| entry.path.into_bytes())
        .collect())
}

/// Write one target tree entry into the worktree and return its index entry —
/// the shared materialization step for every checkout/reset worktree rebuild.
///
/// Gitlinks (mode 160000) never touch the object database: their oid names a
/// commit in the *submodule's* repository, not an object here. Upstream
/// (entry.c `write_entry` S_IFGITLINK) just mkdirs the path — an
/// already-populated submodule is left untouched (EEXIST is success) — and
/// records the oid in the index with a zeroed stat so status re-evaluates the
/// gitlink against the embedded repository's HEAD.
fn materialize_tree_entry(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, path)?;
        fs::create_dir_all(&dir_path)?;
        return Ok(IndexEntry {
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
        });
    }
    let file_path = write_worktree_blob_entry(db, worktree_root, path, entry)?;
    let metadata = fs::symlink_metadata(&file_path)?;
    let mut index_entry = index_entry_from_metadata(path.to_vec(), entry.oid, &metadata);
    index_entry.mode = entry.mode;
    Ok(index_entry)
}

/// Materialize a blob (or symlink) tree entry into the worktree at `path`,
/// returning the absolute path written. Shared by every checkout/reset worktree
/// rebuild so the type-change handling is identical everywhere.
///
/// Mirrors git's entry.c `write_entry`: it unlinks whatever currently occupies
/// the path before creating the new object, so a type transition (regular file ⇄
/// symlink, or a stale symlink/directory in the way) is overwritten rather than
/// left in place or failing with EEXIST. A plain `fs::write` follows an existing
/// symlink and would write *through* it (leaving the link), so the unlink is
/// load-bearing for the symlink-stash / reset-hard type-change cases.
fn write_worktree_blob_entry(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<PathBuf> {
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    let file_path = worktree_path(worktree_root, path)?;
    // Clear any non-directory blocking an ancestor component (prior tree had
    // `dir` as a FILE, target wants `dir/<child>`), creating the parent dirs.
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    // Clear whatever sits at the leaf — including a directory where the target
    // wants a plain file (reverse D/F) — before writing.
    remove_existing_worktree_path(&file_path)?;
    if (entry.mode & 0o170000) == 0o120000 {
        // Symlink entry (mode 120000): the blob body is the link target.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let target =
                std::path::PathBuf::from(std::ffi::OsString::from_vec(object.body.clone()));
            std::os::unix::fs::symlink(&target, &file_path)?;
        }
        #[cfg(not(unix))]
        fs::write(&file_path, &object.body)?;
    } else {
        fs::write(&file_path, &object.body)?;
        set_worktree_file_mode(&file_path, entry.mode)?;
    }
    Ok(file_path)
}

/// Create the ancestor directories of a worktree blob path, removing any
/// regular file or symlink that occupies an ancestor *component* first.
///
/// Mirrors git's `entry.c` `create_directories`: it walks each path component
/// between `worktree_root` and the leaf and, for each, if a non-directory (a
/// regular file or symlink left by a prior tree where `dir` was a FILE) blocks
/// it, unlinks the blocker before `mkdir`. A plain `fs::create_dir_all` fails
/// with `ENOTDIR`/`EEXIST` on such a D/F transition; this is the directory-side
/// of git's force-checkout D/F clearing.
///
/// `worktree_root` itself is never touched. Only components strictly between the
/// root and the leaf are cleared, matching `create_directories`' `base_dir_len`
/// boundary.
fn prepare_blob_parent_dirs(worktree_root: &Path, file_path: &Path) -> Result<()> {
    let parent = match file_path.parent() {
        Some(parent) => parent,
        None => return Ok(()),
    };
    // Fast path: parent already a directory (the overwhelmingly common case).
    if parent.is_dir() {
        return Ok(());
    }
    // Collect the ancestor chain from worktree_root (exclusive) down to `parent`
    // (inclusive). We can't `create_dir_all` blindly because a non-directory may
    // sit on one of these components; walk them and clear blockers as git does.
    let mut components: Vec<&Path> = Vec::new();
    let mut cursor = Some(parent);
    while let Some(dir) = cursor {
        if dir == worktree_root {
            break;
        }
        components.push(dir);
        cursor = dir.parent();
        if cursor.is_none() {
            break;
        }
    }
    // Walk root → leaf so each parent exists before its child.
    for dir in components.iter().rev() {
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                // A regular file or symlink occupies this component (the prior
                // tree had `dir` as a FILE). Unlink it, then create the dir.
                fs::remove_file(dir)?;
                fs::create_dir(dir)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(dir)?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Remove whatever currently occupies a worktree path before writing a new
/// object there — a symlink (even a dangling one, which `Path::exists` misses),
/// a regular file, or a directory subtree. Uses `symlink_metadata` (lstat) so a
/// symlink is removed as the link, never followed.
fn remove_existing_worktree_path(file_path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(file_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() {
        // A directory in the way of a file (D/F transition) or a populated
        // gitlink: remove the subtree so the file can be created.
        match fs::remove_dir_all(file_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    } else {
        fs::remove_file(file_path)?;
    }
    Ok(())
}

/// chmod a freshly-materialized worktree blob to match its tree/index entry mode.
///
/// `fs::write` truncates an existing file *in place*, preserving its prior
/// permission bits. For a mode-only diff (identical oid, 100644 vs 100755) that
/// leaves the wrong exec bit on disk — which is exactly the `reset --hard` /
/// checkout bug this guards against. git's checkout path unlinks+recreates the
/// file precisely to "get the new one with the right permissions" (entry.c
/// `write_entry`); we instead chmod the just-written file.
///
/// Mirrors the observable result of git's `create_file` (entry.c):
/// `(mode & 0100) ? 0777 : 0666` masked by the standard umask (0022), i.e. 0755
/// for an executable entry and 0644 otherwise. Only regular-file entries (100644
/// / 100755) are chmod'd; gitlinks and symlinks have no meaningful exec bit.
///
/// We set the perms directly (rather than relying on a fresh `open(2)` to apply
/// the umask) because `fs::write` truncates an existing file in place, leaving its
/// old permission bits — the very thing that breaks a mode-only checkout/reset.
/// Matching git's default-umask output keeps the worktree byte-for-byte aligned
/// with the oracle, which is what the parity suite asserts.
#[cfg(unix)]
fn set_worktree_file_mode(file_path: &Path, entry_mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = match entry_mode {
        0o100755 => 0o755,
        0o100644 => 0o644,
        _ => return Ok(()),
    };
    fs::set_permissions(file_path, fs::Permissions::from_mode(perms))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_worktree_file_mode(_file_path: &Path, _entry_mode: u32) -> Result<()> {
    Ok(())
}

/// Materialize a tree object into the index and worktree.
pub fn checkout_tree_to_index_and_worktree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, tree_oid, &mut target_entries)?;

    for path in read_index_entries(git_dir, format)?.keys() {
        if !target_entries.contains_key(path) {
            remove_worktree_file(worktree_root, path)?;
        }
    }

    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        index_entries.push(materialize_tree_entry(&db, worktree_root, path, entry)?);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    fs::write(
        repository_index_path(git_dir),
        Index {
            version: 2,
            entries: index_entries,
            extensions: Vec::new(),
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

pub fn reset_index_to_commit(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<RestoreResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let commit = read_commit(&db, format, commit_oid)?;
    let mut target_entries = BTreeMap::new();
    collect_tree_entries(&db, format, &commit.tree, &mut target_entries)?;
    // git's `reset --mixed` preserves the skip-worktree bit on entries that survive
    // the reset (t7102 "--mixed preserves skip-worktree"): carry it forward from the
    // pre-reset index keyed by path, so reconstructed entries keep CE_SKIP_WORKTREE.
    let index_path = repository_index_path(git_dir);
    let prior_skip_worktree: BTreeSet<Vec<u8>> = match fs::read(&index_path) {
        Ok(bytes) => Index::parse(&bytes, format)?
            .entries
            .iter()
            .filter(|entry| entry.is_skip_worktree())
            .map(|entry| entry.path.as_bytes().to_vec())
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(err) => return Err(err.into()),
    };
    let mut index_entries = Vec::new();
    for (path, entry) in &target_entries {
        let mut restored = restored_head_index_entry(worktree_root, &db, path, entry)?;
        if prior_skip_worktree.contains(path) {
            restored.set_skip_worktree(true);
        }
        index_entries.push(restored);
    }
    index_entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut index = Index {
        version: 2,
        entries: index_entries,
        extensions: Vec::new(),
        checksum: None,
    };
    index.upgrade_version_for_flags();
    fs::write(&index_path, index.write(format)?)?;
    Ok(RestoreResult {
        restored: target_entries.len(),
    })
}

/// Build a fresh in-memory index that mirrors the tree `tree_oid`, the way
/// `git read-tree <tree>` does: every blob, symlink, and gitlink leaf (found by
/// recursing subtrees) becomes a stage-0 entry carrying the tree mode and oid,
/// with a fully zeroed stat (so nothing is treated as stat-clean) and size 0.
/// Entries are sorted by path; the index is version 2 with no extensions.
///
/// This does not touch the worktree or write anything to disk — serialize the
/// result with [`Index::write`] (and persist it) when you want to replace
/// `.git/index`.
pub fn index_from_tree(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Index> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    if *tree_oid != ObjectId::empty_tree(format) {
        let mut tree_entries = BTreeMap::new();
        collect_tree_entries(db, format, tree_oid, &mut tree_entries)?;
        entries.reserve(tree_entries.len());
        for (path, entry) in tree_entries {
            let name_len = (path.len().min(0x0fff)) as u16;
            entries.push(IndexEntry {
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
                flags: name_len,
                flags_extended: 0,
                path: path.into(),
            });
        }
    }
    // git orders index entries by path bytes; BTreeMap already yields that, but
    // sort explicitly so the contract holds regardless of how entries arrive.
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Index {
        version: 2,
        entries,
        extensions: Vec::new(),
        checksum: None,
    })
}

/// Enforces a [`SparseCheckout`] against the current index and worktree.
///
/// Every stage-0 index entry is classified with the sparse patterns (see
/// [`SparseCheckoutMode`] for the matching semantics):
///
/// * **In cone**: the skip-worktree bit is cleared and, if the worktree file is
///   missing, it is re-materialized from the entry's blob in the object
///   database. Existing worktree files are left untouched so local content is
///   preserved.
/// * **Out of cone**: the skip-worktree bit is set and any existing worktree
///   file is removed (empty parent directories are pruned).
///
/// Returns `true` when `path` is inside the sparse-checkout described by
/// `sparse` under the given matching `mode`. This is the engine behind
/// `git sparse-checkout check-rules`: a path is "in" the sparse-checkout when
/// the compiled matcher would keep its worktree file. Cone and full (gitignore)
/// grammars are both handled, exactly as the apply engine interprets them, so
/// `check-rules` and `set`/`reapply` agree by construction.
pub fn path_in_sparse_checkout(path: &[u8], sparse: &SparseCheckout, mode: SparseCheckoutMode) -> bool {
    SparseMatcher::new(sparse, mode).includes_file(path)
}

/// Conflicted entries (stage != 0) are never given the skip-worktree bit and
/// are left alone, matching upstream Git. The index is rewritten in place.
pub fn apply_sparse_checkout(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    sparse: &SparseCheckout,
) -> Result<ApplySparseResult> {
    apply_sparse_checkout_with_mode(
        worktree_root,
        git_dir,
        format,
        sparse,
        SparseCheckoutMode::Auto,
    )
}

/// Like [`apply_sparse_checkout`] but lets the caller force the pattern
/// interpretation instead of auto-detecting it.
pub fn apply_sparse_checkout_with_mode(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    sparse: &SparseCheckout,
    mode: SparseCheckoutMode,
) -> Result<ApplySparseResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let index_path = repository_index_path(git_dir);
    let mut index = if index_path.exists() {
        Index::parse(&fs::read(&index_path)?, format)?
    } else {
        return Ok(ApplySparseResult {
            materialized: Vec::new(),
            skipped: Vec::new(),
            not_up_to_date: Vec::new(),
        });
    };
    let matcher = SparseMatcher::new(sparse, mode);
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    // Expand any collapsed sparse-directory entries to a full index before we
    // reconcile per-path: the apply loop reasons about individual blob paths, so
    // it must never see a sparse-dir entry. (Re-collapse happens at the end when
    // a sparse index is requested.)
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        expand_sparse_index(&mut index, &db, format)?;
    }
    let mut materialized = Vec::new();
    let mut skipped = Vec::new();
    let mut not_up_to_date = Vec::new();
    for entry in &mut index.entries {
        // Never touch conflicted entries.
        if index_entry_stage(entry) != 0 {
            continue;
        }
        if matcher.includes_file(entry.path.as_bytes()) {
            clear_skip_worktree(entry);
            let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
            if !file_path.exists() {
                materialize_index_entry_file(&db, worktree_root, &file_path, entry)?;
            }
            materialized.push(entry.path.as_bytes().to_vec());
        } else {
            // The path is out of cone, so its worktree file should be removed and
            // the entry marked skip-worktree. But git refuses to delete a file
            // that is *not up to date* with the index (e.g. one that reappeared in
            // the worktree after the path was already sparse): it leaves the file,
            // leaves the skip-worktree bit clear, and reports the path in its "not
            // up to date" warning. Mirror that to avoid silent data loss.
            let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
            match fs::symlink_metadata(&file_path) {
                Ok(metadata) if !worktree_entry_is_uptodate(entry, &metadata) => {
                    clear_skip_worktree(entry);
                    not_up_to_date.push(entry.path.as_bytes().to_vec());
                }
                _ => {
                    set_skip_worktree(entry);
                    remove_worktree_file(worktree_root, entry.path.as_bytes())?;
                    skipped.push(entry.path.as_bytes().to_vec());
                }
            }
        }
    }
    not_up_to_date.sort();
    normalize_index_version_for_extended_flags(&mut index);
    // When a sparse index was requested (cone mode + index.sparse), collapse the
    // fully-out-of-cone directories into single sparse-directory entries and
    // mark the index with the `sdir` extension. Otherwise ensure the index is
    // written full (and any prior `sdir` marker is cleared).
    if sparse.sparse_index {
        collapse_to_sparse_index(&mut index, &matcher, &db, format)?;
    } else {
        index.clear_sparse_extension()?;
    }
    fs::write(index_path, index.write(format)?)?;
    Ok(ApplySparseResult {
        materialized,
        skipped,
        not_up_to_date,
    })
}

/// Expands every sparse-directory entry in `index` back into the full set of
/// blob (and nested-directory) entries it collapses, reading each directory's
/// tree from `db`. After this the index contains no sparse-directory entries and
/// carries no `sdir` marker — it is a full index that any per-path command can
/// operate on without sparse-index awareness.
///
/// This is the **close-the-class** primitive: a command never needs to special-
/// case a sparse index, because the moment it loads the index it expands to the
/// full form. The collapsed shape is purely an on-disk storage optimization.
pub fn expand_sparse_index(
    index: &mut Index,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<bool> {
    if !index.entries.iter().any(IndexEntry::is_sparse_dir) {
        // Still strip a stray `sdir` marker so the written index is recorded full.
        let had_marker = index.is_sparse();
        index.clear_sparse_extension()?;
        return Ok(had_marker);
    }
    let mut expanded: Vec<IndexEntry> = Vec::with_capacity(index.entries.len());
    for entry in std::mem::take(&mut index.entries) {
        if !entry.is_sparse_dir() {
            expanded.push(entry);
            continue;
        }
        // The sparse-dir path ends in `/`; its OID is the directory's tree.
        let dir = entry.path.as_bytes();
        let dir_prefix = dir; // includes the trailing slash
        for (rel, (mode, oid)) in sley_diff_merge::flatten_tree(db, format, &entry.oid)? {
            let mut full_path = dir_prefix.to_vec();
            full_path.extend_from_slice(&rel);
            let mut blob = blank_sparse_blob_entry(format, &full_path, mode, oid);
            // Re-collapsed entries are skip-worktree (they live outside the cone).
            blob.set_skip_worktree(true);
            expanded.push(blob);
        }
    }
    expanded.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    index.entries = expanded;
    index.clear_sparse_extension()?;
    normalize_index_version_for_extended_flags(index);
    Ok(true)
}

/// Builds a minimal index entry for an expanded sparse blob: zeroed stat fields
/// (the file is not in the worktree), the given mode/oid, and a fresh name
/// length. Stat fields are zero because a skip-worktree file has no on-disk
/// presence to record.
fn blank_sparse_blob_entry(
    format: ObjectFormat,
    path: &[u8],
    mode: u32,
    oid: ObjectId,
) -> IndexEntry {
    let _ = format;
    let mut entry = IndexEntry {
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
        path: path.into(),
    };
    entry.refresh_name_length();
    entry
}

/// Collapses fully-out-of-cone directories in `index` into single sparse-
/// directory entries (mode `040000`, skip-worktree, the directory tree's OID),
/// then marks the index with the `sdir` extension. A directory is collapsible
/// when *every* entry under it is skip-worktree and stage 0 — i.e. nothing in it
/// is in the cone or conflicted. The shallowest such directory subsumes deeper
/// ones, matching git's `convert_to_sparse` cache-tree walk.
fn collapse_to_sparse_index(
    index: &mut Index,
    matcher: &SparseMatcher,
    db: &FileObjectDatabase,
    format: ObjectFormat,
) -> Result<()> {
    // First expand any pre-existing sparse-dir entries so the collapse decision
    // sees a uniform full index (idempotent re-collapse).
    if index.entries.iter().any(IndexEntry::is_sparse_dir) {
        expand_sparse_index(index, db, format)?;
    }

    // Any unmerged (stage != 0) entry forbids a sparse index entirely (the cache
    // tree cannot be built), so stay full — matching git's bail.
    if index.entries.iter().any(|e| index_entry_stage(e) != 0) {
        index.clear_sparse_extension()?;
        return Ok(());
    }

    index
        .entries
        .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    // Determine, for every directory prefix, whether it contains any in-cone
    // path. A directory with no in-cone descendant is collapsible.
    use std::collections::BTreeMap;
    let mut dir_has_in_cone: BTreeMap<Vec<u8>, bool> = BTreeMap::new();
    for entry in &index.entries {
        let path = entry.path.as_bytes();
        let in_cone = matcher.includes_file(path);
        let mut start = 0usize;
        while let Some(rel) = path.get(start..).and_then(|s| s.iter().position(|b| *b == b'/')) {
            let end = start + rel;
            let dir = path[..end].to_vec();
            let flag = dir_has_in_cone.entry(dir).or_insert(false);
            *flag = *flag || in_cone;
            start = end + 1;
        }
    }

    // The collapsible directories are those with no in-cone descendant; keep only
    // the shallowest (a directory whose ancestor is also collapsible is subsumed).
    let collapsible: Vec<Vec<u8>> = {
        let all: Vec<Vec<u8>> = dir_has_in_cone
            .iter()
            .filter(|(_, has)| !**has)
            .map(|(dir, _)| dir.clone())
            .collect();
        all.iter()
            .filter(|dir| {
                !all.iter().any(|other| {
                    other != *dir
                        && dir
                            .strip_prefix(other.as_slice())
                            .is_some_and(|rest| rest.first() == Some(&b'/'))
                })
            })
            .cloned()
            .collect()
    };
    if collapsible.is_empty() {
        index.clear_sparse_extension()?;
        return Ok(());
    }

    let mut checker = db.presence_checker();
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(index.entries.len());
    let mut consumed: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for dir in &collapsible {
        // Gather the entries that live strictly under this directory.
        let mut subtree: Vec<&IndexEntry> = index
            .entries
            .iter()
            .filter(|e| {
                e.path
                    .as_bytes()
                    .strip_prefix(dir.as_slice())
                    .is_some_and(|rest| rest.first() == Some(&b'/'))
            })
            .collect();
        if subtree.is_empty() {
            continue;
        }
        subtree.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        // Build the subtree object and capture its OID.
        let mut prefix = dir.clone();
        prefix.push(b'/');
        let tree_entries: Vec<WriteTreeEntry<'_>> = subtree
            .iter()
            .map(|e| WriteTreeEntry {
                path: e.path.as_bytes(),
                mode: e.mode,
                oid: e.oid.clone(),
            })
            .collect();
        let tree_oid =
            write_tree_entries_stream(&tree_entries, &prefix, None, db, &mut checker, false)?;
        // Mark every consumed path so the second pass drops them.
        for e in &subtree {
            consumed.insert(e.path.as_bytes().to_vec());
        }
        // The sparse-dir entry's name is the directory path WITH a trailing slash.
        let mut sparse_path = dir.clone();
        sparse_path.push(b'/');
        let mut sparse_entry =
            blank_sparse_blob_entry(format, &sparse_path, SPARSE_DIR_MODE, tree_oid);
        sparse_entry.set_skip_worktree(true);
        new_entries.push(sparse_entry);
    }
    // Carry forward every entry that was not collapsed.
    for entry in &index.entries {
        if consumed.contains(entry.path.as_bytes()) {
            continue;
        }
        new_entries.push(entry.clone());
    }
    new_entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    index.entries = new_entries;
    index.set_sparse_extension();
    normalize_index_version_for_extended_flags(index);
    Ok(())
}

/// Whether the worktree file described by `metadata` is up to date with `entry`'s
/// cached index stat, using the size + mtime heuristic at the core of git's
/// `ie_match_stat`. A freshly-checked-out (clean) file matches; a file that was
/// deleted and later recreated — as happens when an out-of-cone path reappears in
/// the worktree — gets a fresh mtime and so reads as modified, which is exactly
/// the state git declines to overwrite during a sparse update.
fn worktree_entry_is_uptodate(entry: &IndexEntry, metadata: &fs::Metadata) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        // Without a usable mtime we cannot prove the file is clean; treat it as
        // not up to date so a present file is never silently discarded.
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

fn worktree_entry_ref_is_uptodate(entry: &IndexEntryRef<'_>, metadata: &fs::Metadata) -> bool {
    if u64::from(entry.size) != metadata.len() {
        return false;
    }
    let Some((mtime_seconds, mtime_nanoseconds)) = file_mtime_parts(metadata) else {
        return false;
    };
    u64::from(entry.mtime_seconds) == mtime_seconds
        && u64::from(entry.mtime_nanoseconds) == mtime_nanoseconds
}

/// The file's modification time split into whole seconds and the sub-second
/// nanosecond remainder, matching how git stores `mtime` in the index.
fn file_mtime_parts(metadata: &fs::Metadata) -> Option<(u64, u64)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_secs(), u64::from(duration.subsec_nanos())))
}

/// Write a git metadata file through a sibling `.lock` file and atomic rename.
///
/// This helper is intended for small repository/worktree metadata files such as
/// `HEAD`, `config.worktree`, or state files under `.git/`. It deliberately does
/// not try to replace object or pack writers, which have their own durability
/// and naming rules.
pub fn write_metadata_file_atomic(
    path: impl AsRef<Path>,
    bytes: &[u8],
    options: AtomicMetadataWriteOptions,
) -> Result<AtomicMetadataWriteResult> {
    let path = path.as_ref();
    let parent = path.parent().ok_or_else(|| {
        GitError::InvalidPath(format!("metadata path has no parent: {}", path.display()))
    })?;
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = metadata_lock_path(path)?;
    let mut lock = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(GitError::Transaction(format!(
                "metadata lock already exists: {}",
                lock_path.display()
            )));
        }
        Err(err) => return Err(err.into()),
    };
    if let Err(err) = lock.write_all(bytes) {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    if options.fsync_file
        && let Err(err) = lock.sync_all()
    {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    drop(lock);
    if let Err(err) = fs::rename(&lock_path, path) {
        let _ = fs::remove_file(&lock_path);
        return Err(err.into());
    }
    if options.fsync_dir
        && let Ok(dir) = fs::File::open(parent)
    {
        dir.sync_all()?;
    }
    let metadata = fs::metadata(path)?;
    Ok(AtomicMetadataWriteResult {
        path: path.to_path_buf(),
        len: metadata.len(),
        mtime: file_mtime_parts(&metadata),
    })
}

fn metadata_lock_path(path: &Path) -> Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        GitError::InvalidPath(format!("metadata path has no filename: {}", path.display()))
    })?;
    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");
    Ok(path.with_file_name(lock_name))
}

/// Checks out `target` like [`checkout_detached`], but materializes the
/// worktree through the supplied [`SparseCheckout`]: out-of-cone paths are not
/// written, get their skip-worktree bit set, and have any stale worktree file
/// removed. Existing public checkout entry points are unchanged; this is an
/// additive sparse-aware variant.
///
/// The pattern interpretation is auto-detected ([`SparseCheckoutMode::Auto`]);
/// to reconcile an existing checkout under an explicit mode use
/// [`apply_sparse_checkout_with_mode`].
pub fn checkout_detached_sparse(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    target: &ObjectId,
    committer: Vec<u8>,
    message: Vec<u8>,
    sparse: &SparseCheckout,
) -> Result<CheckoutResult> {
    let worktree_root = worktree_root.as_ref();
    let git_dir = git_dir.as_ref();
    let files = checkout_commit_to_index_and_worktree_sparse(
        worktree_root,
        git_dir,
        format,
        target,
        Some((sparse, SparseCheckoutMode::Auto)),
    )?;
    let refs = FileRefStore::new(git_dir, format);
    let zero = ObjectId::null(format);
    let mut tx = refs.transaction();
    tx.update(RefUpdate {
        name: "HEAD".into(),
        expected: None,
        new: RefTarget::Direct(*target),
        reflog: Some(ReflogEntry {
            old_oid: zero,
            new_oid: *target,
            committer,
            message,
        }),
    });
    tx.commit()?;
    Ok(CheckoutResult {
        branch: target.to_string(),
        oid: *target,
        files,
    })
}

fn materialize_index_entry_file(
    db: &FileObjectDatabase,
    worktree_root: &Path,
    file_path: &Path,
    entry: &IndexEntry,
) -> Result<()> {
    // A gitlink (mode 160000) has no blob in this object store and materializes
    // as a directory (git's `write_entry` S_IFGITLINK arm: mkdir, never read an
    // object). Single gitlink rule via `sley_index::is_gitlink`; without it a
    // sparse re-materialization of a submodule path would fail with "not found:
    // blob object <commit-oid>".
    if sley_index::is_gitlink(entry.mode) {
        prepare_blob_parent_dirs(worktree_root, file_path)?;
        fs::create_dir_all(file_path)?;
        return Ok(());
    }
    let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
    prepare_blob_parent_dirs(worktree_root, file_path)?;
    remove_existing_worktree_path(file_path)?;
    fs::write(file_path, &object.body)?;
    set_worktree_file_mode(file_path, entry.mode)?;
    Ok(())
}

fn set_skip_worktree(entry: &mut IndexEntry) {
    entry.flags |= INDEX_FLAG_EXTENDED;
    entry.flags_extended |= INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
}

fn clear_skip_worktree(entry: &mut IndexEntry) {
    entry.flags_extended &= !INDEX_EXTENDED_FLAG_SKIP_WORKTREE;
    if entry.flags_extended == 0 {
        entry.flags &= !INDEX_FLAG_EXTENDED;
    }
}

pub fn restore_worktree_paths_from_head(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    restore_worktree_paths_from_entries(worktree_root, &db, index, &head_entries, paths)
}

pub fn restore_worktree_paths_from_tree(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let source_entries = tree_entries(&db, format, tree_oid)?;
    restore_worktree_paths_from_entries(worktree_root, &db, index, &source_entries, paths)
}

fn restore_worktree_paths_from_entries(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    index: Index,
    source_entries: &BTreeMap<Vec<u8>, TrackedEntry>,
    paths: &[PathBuf],
) -> Result<RestoreResult> {
    let index_entries = index
        .entries
        .into_iter()
        .map(|entry| entry.path.into_bytes())
        .collect::<BTreeSet<_>>();
    let mut restored = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        let git_path = git_path_bytes(relative)?;
        let recursive = path == Path::new(".")
            || path.to_string_lossy().ends_with('/')
            || absolute.is_dir()
            || index_entries
                .iter()
                .any(|entry| index_entry_is_under_path(entry, &git_path))
            || source_entries
                .keys()
                .any(|entry| index_entry_is_under_path(entry, &git_path));
        let mut matched_paths = BTreeSet::new();
        for path in index_entries.iter().chain(source_entries.keys()) {
            if path.as_slice() == git_path.as_slice()
                || (recursive && index_entry_is_under_path(path, &git_path))
            {
                matched_paths.insert(path.clone());
            }
        }
        if matched_paths.is_empty() {
            eprintln!(
                "error: pathspec '{}' did not match any file(s) known to git",
                path.display()
            );
            return Err(GitError::Exit(1));
        }
        for path in matched_paths {
            if let Some(entry) = source_entries.get(&path) {
                restore_head_entry_to_worktree(worktree_root, db, &path, entry)?;
            } else {
                remove_worktree_file(worktree_root, &path)?;
            }
            restored.insert(path);
        }
    }
    Ok(RestoreResult {
        restored: restored.len(),
    })
}

pub fn remove_index_and_worktree_paths(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    paths: &[PathBuf],
    options: RemoveOptions,
    config_parameters_env: Option<&str>,
) -> Result<RemoveResult> {
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
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    let head_entries = head_tree_entries(git_dir, format, &db)?;
    // Stat cache for the local-modification check (git's `ie_match_stat`):
    // proves a path unchanged from the cached stat without reading its blob, so
    // a `git rm --cached` of an untouched path whose blob was removed still
    // succeeds (cf. t1450-fsck cell 90). (`sley_index::IndexStatCache` is a
    // distinct type from this crate's same-named probe helper above.)
    let rm_stat_cache = sley_index::IndexStatCache::from_index(&index, &index_path);
    let Index {
        version: index_version,
        entries: index_entry_list,
        extensions: index_extensions,
        ..
    } = index;
    // The set of distinct index paths (any stage) — used for membership tests.
    let index_paths: BTreeSet<Vec<u8>> = index_entry_list
        .iter()
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();
    // Paths selected for removal. A single selected path removes ALL of its
    // stage entries (so resolving an unmerged path by removal drops stages
    // 1/2/3 together), matching git's name-keyed removal.
    let mut selected = BTreeSet::new();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            worktree_root.join(path)
        };
        let relative = absolute.strip_prefix(worktree_root).map_err(|_| {
            GitError::InvalidPath(format!("path {} is outside worktree", path.display()))
        })?;
        // A pathspec with a trailing slash (e.g. `git rm dir/`) only matches a
        // directory: it must never match a same-named tracked file. `Path`'s
        // component iterator drops the slash, so capture it before it is lost.
        let has_trailing_slash = path_has_trailing_separator(&absolute);
        let git_path = git_path_bytes(relative)?;
        if !has_trailing_slash && index_paths.contains(&git_path) {
            selected.insert(git_path);
            continue;
        }
        // A wildcard pathspec (e.g. `git rm "*"` or `git rm "dir/*.c"`) matches
        // index entries by git's pathspec matcher rather than by literal path or
        // directory prefix. Try the glob match first when the spec contains
        // wildcard metacharacters; a glob match removes the entries directly
        // (no `-r` needed — the pathspec already names the files).
        if pathspec_is_glob(&git_path) {
            let glob_matched = index_paths
                .iter()
                .filter(|entry| {
                    pathspec_item_matches(&git_path, entry, PathspecMatchMagic::default())
                })
                .cloned()
                .collect::<Vec<_>>();
            if !glob_matched.is_empty() {
                selected.extend(glob_matched);
                continue;
            }
            if options.ignore_unmatch {
                continue;
            }
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                String::from_utf8_lossy(&git_path)
            );
            return Err(GitError::Exit(128));
        }
        let matched = index_paths
            .iter()
            .filter(|entry| index_entry_is_under_path(entry, &git_path))
            .cloned()
            .collect::<Vec<_>>();
        if matched.is_empty() {
            if options.ignore_unmatch {
                continue;
            }
            eprintln!(
                "fatal: pathspec '{}' did not match any files",
                String::from_utf8_lossy(&git_path)
            );
            return Err(GitError::Exit(128));
        }
        if !options.recursive {
            eprintln!(
                "fatal: not removing '{}' recursively without -r",
                String::from_utf8_lossy(&git_path)
            );
            return Err(GitError::Exit(128));
        }
        selected.extend(matched);
    }

    // `git rm` runs the local-modification safety check unless `-f` is given —
    // even for `--cached`. The check (a faithful port of builtin/rm.c's
    // `check_local_mod`) buckets each selected path into one of three error
    // classes and prints all of them at once (collected, not fail-fast), so a
    // single `git rm a b c` reports every offending path. See the message
    // assertions in t3600-rm.sh.
    if !options.force {
        let config =
            sley_config::read_repo_config(git_dir, config_parameters_env).unwrap_or_default();
        // advice.rmhints (default true) gates the parenthetical "(use ...)" hints.
        let show_hints = config.get_bool("advice", None, "rmhints").unwrap_or(true);
        // Map each selected path to its stage-0 index entry for the check; an
        // unmerged path (no stage 0) is skipped, exactly like git's loop
        // (index_name_pos fails, and a non-gitlink ours entry `continue`s).
        let stage0: BTreeMap<&[u8], &IndexEntry> = index_entry_list
            .iter()
            .filter(|entry| entry.stage() == Stage::Normal)
            .map(|entry| (entry.path.as_bytes(), entry))
            .collect();
        let mut files_staged: Vec<&[u8]> = Vec::new();
        let mut files_cached: Vec<&[u8]> = Vec::new();
        let mut files_local: Vec<&[u8]> = Vec::new();
        for path in &selected {
            let Some(index_entry) = stage0.get(path.as_slice()) else {
                // Unmerged path with no stage-0 entry: resolving by removal is
                // safe and not warning-worthy.
                continue;
            };
            let worktree_file = worktree_path(worktree_root, path)?;
            // Is the worktree path different from the index?
            //
            // Mirror builtin/rm.c's `check_local_mod`: when `lstat` fails with a
            // "missing file" error (ENOENT *or* ENOTDIR — the path vanished, or a
            // leading component became a file) the file has already gone from the
            // working tree, so git `continue`s and never buckets the path. Same
            // for a tracked plain path that is now a directory on disk: git
            // treats that as ENOENT and skips it (the later worktree-removal step
            // is what fails on a non-empty directory).
            let local_changes = match fs::symlink_metadata(&worktree_file) {
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) || err.raw_os_error() == Some(20) =>
                {
                    // ENOENT/ENOTDIR: already gone — not warning-worthy.
                    continue;
                }
                Err(err) => return Err(err.into()),
                Ok(meta) if meta.is_dir() => continue,
                Ok(meta) => {
                    // git refreshes the index before `check_local_mod`, so a path
                    // whose stat changed but whose content is unchanged is up to
                    // date. We mirror that: a clean cached stat short-circuits to
                    // "unchanged"; otherwise re-hash the (clean-filtered) worktree
                    // content and compare to the index entry's *cached oid* (git's
                    // refresh `hash_object`), NOT the stored blob. Comparing to the
                    // oid — not the blob bytes — means a removed object does not
                    // abort the check (the worktree may still hash to the cached
                    // oid), so `git rm --cached` of a path whose blob was deleted
                    // still succeeds.
                    match rm_stat_cache.index_entry_worktree_stat_verdict(index_entry, &meta) {
                        sley_index::StatVerdict::Clean => false,
                        sley_index::StatVerdict::Dirty
                        | sley_index::StatVerdict::RacyNeedsContentCheck => {
                            let worktree_bytes = apply_clean_filter(
                                worktree_root,
                                git_dir,
                                &config,
                                path,
                                &fs::read(&worktree_file)?,
                            )?;
                            let worktree_oid = EncodedObject::new(ObjectType::Blob, worktree_bytes)
                                .object_id(format)?;
                            worktree_oid != index_entry.oid
                        }
                    }
                }
            };
            // Is the index different from the HEAD commit? (Before the first
            // commit, anything staged is treated as changed from HEAD.)
            let staged_changes = match head_entries.get(path) {
                Some(head_entry) => {
                    head_entry.oid != index_entry.oid || head_entry.mode != index_entry.mode
                }
                None => true,
            };
            if local_changes && staged_changes {
                // `git rm --cached` of an intent-to-add entry is safe.
                if !options.cached || !index_entry.is_intent_to_add() {
                    files_staged.push(path);
                }
            } else if !options.cached {
                if staged_changes {
                    files_cached.push(path);
                }
                if local_changes {
                    files_local.push(path);
                }
            }
        }
        let mut errs = false;
        print_rm_error_files(
            &files_staged,
            "the following file has staged content different from both the\nfile and the HEAD:",
            "the following files have staged content different from both the\nfile and the HEAD:",
            "\n(use -f to force removal)",
            show_hints,
            &mut errs,
        );
        print_rm_error_files(
            &files_cached,
            "the following file has changes staged in the index:",
            "the following files have changes staged in the index:",
            "\n(use --cached to keep the file, or -f to force removal)",
            show_hints,
            &mut errs,
        );
        print_rm_error_files(
            &files_local,
            "the following file has local modifications:",
            "the following files have local modifications:",
            "\n(use --cached to keep the file, or -f to force removal)",
            show_hints,
            &mut errs,
        );
        if errs {
            return Err(GitError::Exit(1));
        }
    }

    if options.dry_run {
        return Ok(RemoveResult {
            removed: selected.into_iter().collect(),
        });
    }
    // Mirror builtin/rm.c's ordering: remove the worktree files BEFORE writing
    // the new index. If the very first removal fails (and nothing has been
    // removed yet), abort without committing the index, so a `git rm d` where
    // `d` is now a non-empty directory fails AND leaves the index untouched.
    // Once any file has been removed we commit to finishing (git does the same).
    if !options.cached {
        let mut removed_any = false;
        for path in &selected {
            match remove_tracked_worktree_path(worktree_root, path)? {
                true => removed_any = true,
                false if !removed_any => {
                    eprintln!(
                        "fatal: git rm: '{}': Is a directory",
                        String::from_utf8_lossy(path)
                    );
                    return Err(GitError::Exit(128));
                }
                false => {}
            }
        }
    }
    // Keep every entry whose path was not selected, preserving original order
    // and all stages of unmerged paths that were not removed.
    let entries = index_entry_list
        .into_iter()
        .filter(|entry| !selected.contains(entry.path.as_bytes()))
        .collect::<Vec<_>>();
    // Removing entries invalidates the cache-tree (`TREE` extension): a stale
    // cached subtree id makes `git diff --cached`/`git status` short-circuit the
    // comparison of an affected directory against HEAD and miss the deletion
    // (observed: `git rm dir/nested.txt` left a valid `dir/` cache-tree, so the
    // deletion never showed in the cached diff). Git invalidates the cache-tree
    // on any index mutation; drop it so it is rebuilt on the next write, exactly
    // like the `add` path does above.
    let extensions = index_extensions_without_cache_tree(&index_extensions);
    fs::write(
        index_path,
        Index {
            version: index_version,
            entries,
            extensions,
            checksum: None,
        }
        .write(format)?,
    )?;
    Ok(RemoveResult {
        removed: selected.into_iter().collect(),
    })
}

/// Remove a tracked path from the working tree, mirroring builtin/rm.c's
/// `remove_path`: unlink the file and prune now-empty parent directories.
/// Returns `Ok(true)` when a file was removed, `Ok(false)` when the path could
/// not be unlinked because it is a directory (the caller decides whether that
/// aborts the run). A path that has already vanished is a no-op success.
fn remove_tracked_worktree_path(root: &Path, path: &[u8]) -> Result<bool> {
    let file = worktree_path(root, path)?;
    match fs::symlink_metadata(&file) {
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(true);
        }
        Err(err) if err.raw_os_error() == Some(20) => return Ok(true), // ENOTDIR
        Err(err) => return Err(err.into()),
        // A directory in the worktree where a plain file is tracked cannot be
        // unlinked (git's remove_path fails on EISDIR). Report it so the caller
        // can abort the removal without committing the index.
        Ok(meta) if meta.is_dir() => return Ok(false),
        Ok(_) => {}
    }
    fs::remove_file(&file)?;
    prune_empty_parents(root, file.parent())?;
    Ok(true)
}

/// Print one batched `git rm` safety error block (mirrors builtin/rm.c's
/// `print_error_files`): the main message, the indented list of offending
/// paths, and — when `advice.rmhints` is enabled — the trailing hint. Sets
/// `*errs` so the caller can fail after collecting every class.
fn print_rm_error_files(
    files: &[&[u8]],
    singular: &str,
    plural: &str,
    hint: &str,
    show_hints: bool,
    errs: &mut bool,
) {
    if files.is_empty() {
        return;
    }
    let mut message = String::from(if files.len() == 1 { singular } else { plural });
    for path in files {
        message.push_str("\n    ");
        message.push_str(&String::from_utf8_lossy(path));
    }
    if show_hints {
        message.push_str(hint);
    }
    eprintln!("error: {message}");
    *errs = true;
}

pub fn move_index_and_worktree_path(
    worktree_root: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    format: ObjectFormat,
    source: &Path,
    destination: &Path,
    options: MoveOptions,
) -> Result<MoveResult> {
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
    let source_absolute = if source.is_absolute() {
        source.to_path_buf()
    } else {
        worktree_root.join(source)
    };
    let destination_absolute = if destination.is_absolute() {
        destination.to_path_buf()
    } else {
        worktree_root.join(destination)
    };
    let destination_absolute = if destination_absolute.is_dir() {
        let Some(file_name) = source_absolute.file_name() else {
            return Err(GitError::InvalidPath(format!(
                "invalid source path {}",
                source.display()
            )));
        };
        destination_absolute.join(file_name)
    } else {
        destination_absolute
    };
    let source_relative = source_absolute.strip_prefix(worktree_root).map_err(|_| {
        GitError::InvalidPath(format!("path {} is outside worktree", source.display()))
    })?;
    let destination_relative = destination_absolute
        .strip_prefix(worktree_root)
        .map_err(|_| {
            GitError::InvalidPath(format!(
                "path {} is outside worktree",
                destination.display()
            ))
        })?;
    let source_path = git_path_bytes(source_relative)?;
    let destination_path = git_path_bytes(destination_relative)?;
    let destination_has_trailing_separator = path_has_trailing_separator(&destination_absolute);
    if destination_has_trailing_separator && !destination_absolute.is_dir() {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        let mut destination = String::from_utf8_lossy(&destination_path).into_owned();
        destination.push('/');
        if options.dry_run {
            let fatal = format!(
                "fatal: destination directory does not exist, source={}, destination={destination}",
                String::from_utf8_lossy(&source_path),
            );
            return Ok(MoveResult {
                source: source_path,
                destination: destination.clone().into_bytes(),
                skipped: false,
                fatal: Some(fatal),
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: destination directory does not exist, source={}, destination={destination}",
            String::from_utf8_lossy(&source_path),
        );
        return Err(GitError::Exit(128));
    }
    if destination_absolute.exists() {
        if !options.force {
            if options.skip_errors {
                return Ok(MoveResult {
                    source: source_path,
                    destination: destination_path,
                    skipped: true,
                    fatal: None,
                    details: Vec::new(),
                });
            }
            if options.dry_run {
                let fatal = format!(
                    "fatal: destination exists, source={}, destination={}",
                    String::from_utf8_lossy(&source_path),
                    String::from_utf8_lossy(&destination_path)
                );
                return Ok(MoveResult {
                    source: source_path,
                    destination: destination_path,
                    skipped: false,
                    fatal: Some(fatal),
                    details: Vec::new(),
                });
            }
            eprintln!(
                "fatal: destination exists, source={}, destination={}",
                String::from_utf8_lossy(&source_path),
                String::from_utf8_lossy(&destination_path)
            );
            return Err(GitError::Exit(128));
        }
        if !options.dry_run && destination_absolute.is_dir() {
            fs::remove_dir_all(&destination_absolute)?;
        } else if !options.dry_run {
            fs::remove_file(&destination_absolute)?;
        }
    }
    let directory_prefix = {
        let mut prefix = source_path.clone();
        prefix.push(b'/');
        prefix
    };
    let directory_entries: Vec<_> = index
        .entries
        .iter()
        .filter(|entry| entry.path.as_bytes().starts_with(&directory_prefix))
        .cloned()
        .collect();
    if !directory_entries.is_empty() {
        let details: Vec<_> = directory_entries
            .iter()
            .map(|entry| {
                let suffix = &entry.path.as_bytes()[source_path.len()..];
                let mut destination = destination_path.clone();
                destination.extend_from_slice(suffix);
                MoveDetail {
                    source: entry.path.as_bytes().to_vec(),
                    destination,
                    skipped: false,
                }
            })
            .collect();
        if options.dry_run {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: false,
                fatal: None,
                details,
            });
        }
        fs::rename(&source_absolute, &destination_absolute)?;
        let moved_paths: Vec<_> = details
            .iter()
            .map(|detail| detail.destination.clone())
            .collect();
        index.entries.retain(|entry| {
            !entry.path.as_bytes().starts_with(&directory_prefix)
                && !moved_paths
                    .iter()
                    .any(|m| m.as_slice() == entry.path.as_bytes())
        });
        for (source_entry, detail) in directory_entries.into_iter().zip(details.iter()) {
            let relative_path = git_path_to_relative_path(&detail.destination)?;
            let metadata = fs::metadata(worktree_root.join(relative_path))?;
            let mut destination_entry =
                index_entry_from_metadata(detail.destination.clone(), source_entry.oid, &metadata);
            destination_entry.mode = source_entry.mode;
            index.entries.push(destination_entry);
        }
        index
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        index.extensions.clear();
        fs::write(index_path, index.write(format)?)?;
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details,
        });
    }

    let Some(position) = index
        .entries
        .iter()
        .position(|entry| entry.path == source_path)
    else {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        let source_kind = if source_absolute.exists() {
            "not under version control"
        } else {
            "bad source"
        };
        if options.dry_run {
            let fatal = format!(
                "fatal: {source_kind}, source={}, destination={}",
                String::from_utf8_lossy(&source_path),
                String::from_utf8_lossy(&destination_path)
            );
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: false,
                fatal: Some(fatal),
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: {source_kind}, source={}, destination={}",
            String::from_utf8_lossy(&source_path),
            String::from_utf8_lossy(&destination_path)
        );
        return Err(GitError::Exit(128));
    };
    if options.dry_run {
        return Ok(MoveResult {
            source: source_path,
            destination: destination_path,
            skipped: false,
            fatal: None,
            details: Vec::new(),
        });
    }
    if let Some(parent) = destination_absolute.parent()
        && !parent.exists()
    {
        if options.skip_errors {
            return Ok(MoveResult {
                source: source_path,
                destination: destination_path,
                skipped: true,
                fatal: None,
                details: Vec::new(),
            });
        }
        eprintln!(
            "fatal: renaming '{}' failed: No such file or directory",
            String::from_utf8_lossy(&source_path)
        );
        return Err(GitError::Exit(128));
    }
    fs::rename(&source_absolute, &destination_absolute)?;
    let metadata = fs::metadata(&destination_absolute)?;
    let source_entry = index.entries.remove(position);
    let mut destination_entry =
        index_entry_from_metadata(destination_path.clone(), source_entry.oid, &metadata);
    destination_entry.mode = source_entry.mode;
    index.entries.retain(|entry| entry.path != destination_path);
    index.entries.push(destination_entry);
    index
        .entries
        .sort_by(|left, right| left.path.cmp(&right.path));
    index.extensions.clear();
    fs::write(index_path, index.write(format)?)?;
    Ok(MoveResult {
        source: source_path,
        destination: destination_path,
        skipped: false,
        fatal: None,
        details: Vec::new(),
    })
}

fn restore_index_entry(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
    entry: &IndexEntry,
    smudge_config: Option<&GitConfig>,
) -> Result<()> {
    // A gitlink (mode 160000) names a commit in the submodule's repository, not
    // a blob here — reading it as a blob fails ("not found: blob object"). git's
    // `checkout_entry` S_IFGITLINK arm just ensures the submodule directory
    // exists and never touches an object; the submodule's content is `submodule
    // update` territory. Single gitlink rule via `sley_index::is_gitlink`.
    if sley_index::is_gitlink(entry.mode) {
        let dir_path = worktree_path(worktree_root, entry.path.as_bytes())?;
        fs::create_dir_all(&dir_path)?;
        return Ok(());
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
            apply_smudge_filter_with_attributes_cow(
                config,
                &checks,
                entry.path.as_bytes(),
                &object.body,
            )?
        }
        None => Cow::Borrowed(&object.body),
    };
    let file_path = worktree_path(worktree_root, entry.path.as_bytes())?;
    prepare_blob_parent_dirs(worktree_root, &file_path)?;
    remove_existing_worktree_path(&file_path)?;
    fs::write(&file_path, &body)?;
    set_worktree_file_mode(&file_path, entry.mode)?;
    Ok(())
}

fn restored_head_index_entry(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    let file_path = worktree_path(worktree_root, path)?;
    // This restores the index from a tree (reset --mixed / stash / sparse) WITHOUT
    // rewriting the worktree file, so the file on disk may hold different content
    // than `entry.oid`. Crucially we must NOT copy the worktree file's stat onto
    // this entry: that would make the cached stat match a file whose real content
    // hashes to a DIFFERENT oid, breaking git's "stat-match implies oid-match"
    // invariant that the status stat-cache relies on. Leave the stat zeroed so
    // status always re-hashes this path and detects any modification -- exactly
    // git's behavior for tree-sourced entries until a later refresh validates them.
    let size = if sley_index::is_gitlink(entry.mode) {
        // A gitlink's oid names a commit in the submodule's repository — it is
        // not readable here, and a tree-sourced gitlink entry carries size 0.
        0
    } else {
        match fs::metadata(&file_path) {
            Ok(metadata) => metadata.len().min(u32::MAX as u64) as u32,
            Err(_) => {
                let object = read_expected_object(db, &entry.oid, ObjectType::Blob)?;
                object.body.len().min(u32::MAX as usize) as u32
            }
        }
    };
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
        size,
        oid: entry.oid,
        flags: path.len().min(0x0fff) as u16,
        flags_extended: 0,
        path: BString::from(path),
    })
}

fn restore_head_entry_to_worktree(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<()> {
    // Route through the single gitlink-aware materializer: a gitlink has no blob
    // here, so `write_worktree_blob_entry` would fail reading the commit-oid as
    // a blob. `materialize_tree_entry` owns the gitlink-vs-blob decision (mkdir
    // the submodule dir) in ONE place. The returned index entry is unused on
    // this worktree-only restore path.
    materialize_tree_entry(db, worktree_root, path, entry)?;
    Ok(())
}

fn restore_head_entry_to_worktree_and_index(
    worktree_root: &Path,
    db: &FileObjectDatabase,
    path: &[u8],
    entry: &TrackedEntry,
) -> Result<IndexEntry> {
    // Route through the single gitlink-aware materializer rather than calling
    // `write_worktree_blob_entry` directly: a gitlink (mode 160000) has no blob
    // in this object store, so the blob read would fail with "not found: blob
    // object <commit-oid>". `materialize_tree_entry` owns the
    // gitlink-vs-blob/symlink decision (mkdir the submodule dir, never read an
    // object) in ONE place, so `checkout <tree> -- <gitlink-path>` /
    // `restore --source` inherit the same gitlink correctness as `reset --hard`.
    materialize_tree_entry(db, worktree_root, path, entry)
}

fn index_has_entry_under(entries: &[IndexEntry], directory: &[u8]) -> bool {
    entries
        .iter()
        .any(|entry| index_entry_is_under_path(entry.path.as_bytes(), directory))
}

fn index_entry_is_under_path(entry_path: &[u8], directory: &[u8]) -> bool {
    if directory.is_empty() {
        return true;
    }
    entry_path
        .strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix(b"/"))
        .is_some()
}

fn index_entry_from_metadata(
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
    IndexEntry {
        ctime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        ctime_nanoseconds: duration.subsec_nanos(),
        mtime_seconds: duration.as_secs().min(u32::MAX as u64) as u32,
        mtime_nanoseconds: duration.subsec_nanos(),
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: metadata.len().min(u32::MAX as u64) as u32,
        oid,
        flags,
        flags_extended: 0,
        path,
    }
}

fn read_expected_object(
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

fn expect_missing_object_kind(
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

fn missing_kind_for_type(object_type: ObjectType) -> MissingObjectKind {
    match object_type {
        ObjectType::Blob => MissingObjectKind::Blob,
        ObjectType::Tree => MissingObjectKind::Tree,
        ObjectType::Commit => MissingObjectKind::Commit,
        ObjectType::Tag => MissingObjectKind::Tag,
    }
}

fn read_commit(db: &FileObjectDatabase, format: ObjectFormat, oid: &ObjectId) -> Result<Commit> {
    let object = read_expected_object(db, oid, ObjectType::Commit)?;
    Commit::parse(format, &object.body)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedEntry {
    mode: u32,
    oid: ObjectId,
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
struct IndexStatCache {
    entries: HashMap<Vec<u8>, IndexEntry>,
    /// The index file's modification time as `(seconds, nanoseconds)`, or `None`
    /// when it could not be determined. Used as git's racy-clean reference.
    index_mtime: Option<(u64, u64)>,
}

impl IndexStatCache {
    /// Builds the cache from an already-parsed index plus the path of the index
    /// file on disk (whose mtime becomes the racy-clean reference). Only stage-0
    /// entries are retained; higher merge stages never describe a worktree file.
    fn from_index(index: &Index, index_path: &Path) -> Self {
        let index_mtime = fs::metadata(index_path)
            .ok()
            .and_then(|metadata| file_mtime_parts(&metadata));
        Self::from_index_mtime(index, index_mtime)
    }

    fn from_index_mtime(index: &Index, index_mtime: Option<(u64, u64)>) -> Self {
        IndexStatCache {
            entries: stage0_index_entries(index),
            index_mtime,
        }
    }

    fn from_index_mtime_only(index_mtime: Option<(u64, u64)>) -> Self {
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

    /// Returns the cached [`TrackedEntry`] for `git_path` (reusing its stored
    /// oid, so the caller can SKIP reading, filtering, and hashing the file) only
    /// when the worktree file is provably unchanged since it was staged: a
    /// stage-0 entry exists, its recorded mode matches the file's current mode
    /// (catching pure `chmod`s that do not move mtime), the size+mtime stat
    /// check passes, and the entry is not racily clean. Otherwise returns `None`
    /// and the caller hashes the file as usual.
    fn reuse_tracked_entry(
        &self,
        git_path: &[u8],
        worktree_metadata: &fs::Metadata,
    ) -> Option<TrackedEntry> {
        let entry = self.entries.get(git_path)?;
        self.reuse_index_entry(entry, worktree_metadata)
    }

    fn reuse_index_entry(
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

    fn reuse_index_entry_ref(
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

fn read_index_entries(
    git_dir: &Path,
    format: ObjectFormat,
) -> Result<BTreeMap<Vec<u8>, TrackedEntry>> {
    let db = FileObjectDatabase::from_git_dir(git_dir, format);
    Ok(read_index_entries_with_stat_cache(git_dir, format, &db)?.0)
}

fn resolve_head_tree_oid(
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

fn resolve_head_commit_oid(git_dir: &Path, format: ObjectFormat) -> Result<Option<ObjectId>> {
    let refs = FileRefStore::new(git_dir, format);
    sley_refs::resolve_ref_peeled(&refs, "HEAD")
}

fn status_row_is_untracked_or_ignored(entry: ShortStatusRow<'_>) -> bool {
    matches!((entry.index, entry.worktree), (b'?', b'?') | (b'!', b'!'))
}

fn checkout_switch_head_symbolic(
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

fn cache_tree_is_valid(tree: &CacheTree) -> bool {
    if tree.entry_count < 0 || tree.oid.is_none() {
        return false;
    }
    tree.subtrees
        .iter()
        .all(|child| cache_tree_is_valid(&child.tree))
}

fn head_matches_index_from_cache_tree(
    index: &Index,
    format: ObjectFormat,
    head_tree_oid: &ObjectId,
    stage0_entry_count: usize,
) -> Result<bool> {
    let cache_tree = match index.cache_tree(format) {
        Ok(Some(cache_tree)) => cache_tree,
        Ok(None) | Err(_) => return Ok(false),
    };
    if !cache_tree_is_valid(&cache_tree) {
        return Ok(false);
    }
    let Some(root_oid) = cache_tree.oid.as_ref() else {
        return Ok(false);
    };
    if root_oid != head_tree_oid {
        return Ok(false);
    }
    Ok(cache_tree.entry_count as usize == stage0_entry_count)
}

fn head_matches_borrowed_index_from_cache_tree(
    index: &BorrowedIndex<'_>,
    format: ObjectFormat,
    head_tree_oid: &ObjectId,
    stage0_entry_count: usize,
) -> Result<bool> {
    let cache_tree = match index.cache_tree(format) {
        Ok(Some(cache_tree)) => cache_tree,
        Ok(None) | Err(_) => return Ok(false),
    };
    if !cache_tree_is_valid(&cache_tree) {
        return Ok(false);
    }
    let Some(root_oid) = cache_tree.oid.as_ref() else {
        return Ok(false);
    };
    if root_oid != head_tree_oid {
        return Ok(false);
    }
    Ok(cache_tree.entry_count as usize == stage0_entry_count)
}

/// Parses the index a single time and returns both the path -> [`TrackedEntry`]
/// map used for status comparisons AND the [`IndexStatCache`] used to short-cut
/// the worktree walk, avoiding a second parse of the same file.
fn read_index_entries_with_stat_cache(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<(BTreeMap<Vec<u8>, TrackedEntry>, IndexStatCache, bool)> {
    let (index, stat_cache, head_matches_index) = read_index_with_stat_cache(git_dir, format, db)?;
    let tracked = index_entries_from_index(index);
    Ok((tracked, stat_cache, head_matches_index))
}

fn index_entries_from_index(index: Index) -> BTreeMap<Vec<u8>, TrackedEntry> {
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

fn read_index_with_stat_cache(
    git_dir: &Path,
    format: ObjectFormat,
    db: &FileObjectDatabase,
) -> Result<(Index, IndexStatCache, bool)> {
    read_index_with_stat_cache_entries(git_dir, format, db, true)
}

fn read_index_with_stat_cache_entries(
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
    let index = Index::parse(&fs::read(&index_path)?, format)?;
    let index_mtime = file_mtime_parts(&index_metadata);
    let stage0_entry_count = index
        .entries
        .iter()
        .filter(|entry| index_entry_stage(entry) == 0)
        .count();
    let stat_cache = if include_entries {
        IndexStatCache::from_index_mtime(&index, index_mtime)
    } else {
        IndexStatCache::from_index_mtime_only(index_mtime)
    };
    let head_matches_index = match resolve_head_tree_oid(git_dir, format, db)? {
        Some(head_tree_oid) => {
            head_matches_index_from_cache_tree(&index, format, &head_tree_oid, stage0_entry_count)?
        }
        None => false,
    };
    Ok((index, stat_cache, head_matches_index))
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
    let object = read_expected_object(db, &commit_oid, ObjectType::Commit)?;
    let commit = Commit::parse_ref(format, &object.body)?;
    let mut entries = BTreeMap::new();
    collect_tree_entries(db, format, &commit.tree, &mut entries)?;
    Ok(entries)
}

fn tree_entries(
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
fn collect_tree_entries(
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
fn worktree_entries_with_stat_cache(
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
type WorktreeEntriesWithDirt = (BTreeMap<Vec<u8>, TrackedEntry>, BTreeMap<Vec<u8>, u8>);

/// Status worktree snapshot: tracked/untracked entries, gitlink dirt masks, and
/// tracked paths observed in the worktree.
type StatusWorktreeSnapshot = (
    BTreeMap<Vec<u8>, TrackedEntry>,
    BTreeMap<Vec<u8>, u8>,
    HashSet<Vec<u8>>,
);

/// Like [`worktree_entries_with_stat_cache`], but also reports, for every
/// tracked gitlink path whose submodule working tree is dirty, the dirt mask
/// ([`DIRTY_SUBMODULE_MODIFIED`] / [`DIRTY_SUBMODULE_UNTRACKED`]).
fn worktree_entries_with_submodule_dirt(
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

fn status_worktree_entries_with_submodule_dirt(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    stat_cache: &IndexStatCache,
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

fn worktree_entry_for_git_path(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    git_path: &[u8],
    expected_oid: &ObjectId,
    expected_mode: u32,
    stat_cache: Option<&IndexStatCache>,
) -> Result<Option<TrackedEntry>> {
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
        apply_clean_filter(worktree_root, git_dir, &config, git_path, &body)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

fn worktree_entry_for_index_entry_with_attributes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entry: &IndexEntry,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
) -> Result<Option<TrackedEntry>> {
    let git_path = index_entry.path.as_bytes();
    let expected_mode = index_entry.mode;
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
        apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

fn worktree_entry_for_index_entry_ref_with_attributes(
    worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    index_entry: &IndexEntryRef<'_>,
    stat_cache: &IndexStatCache,
    clean_filter: &mut Option<TrackedOnlyCleanFilter>,
) -> Result<Option<TrackedEntry>> {
    let git_path = index_entry.path;
    let expected_mode = index_entry.mode;
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
        apply_clean_filter_with_attributes(&clean_filter.config, &checks, git_path, &body)?
    };
    let oid = EncodedObject::new(ObjectType::Blob, body).object_id(format)?;
    Ok(Some(TrackedEntry { mode, oid }))
}

struct TrackedOnlyCleanFilter {
    config: GitConfig,
    matcher: AttributeMatcher,
    requested: Vec<Vec<u8>>,
    attribute_dirs: BTreeSet<Vec<u8>>,
}

impl TrackedOnlyCleanFilter {
    fn read_attributes_for_path(&mut self, worktree_root: &Path, git_path: &[u8]) -> Result<()> {
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

fn tracked_only_clean_filter<'a>(
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

fn tracked_only_clean_filter_with_config<'a>(
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

struct WorktreeEntriesWalk<'a> {
    git_dir: &'a Path,
    format: ObjectFormat,
    config: &'a GitConfig,
    matcher: &'a mut AttributeMatcher,
    requested: &'a [Vec<u8>],
    stat_cache: Option<&'a IndexStatCache>,
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
            || self
                .tracked_entry_for(git_path)
                .is_none_or(|tracked| tracked != *entry)
    }
}

fn git_path_append_component(parent: &[u8], component: &std::ffi::OsStr) -> Vec<u8> {
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

fn git_path_push_component(path: &mut Vec<u8>, component: &std::ffi::OsStr) -> usize {
    let original_len = path.len();
    let component = os_str_component_bytes(component);
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(component.as_ref());
    original_len
}

#[cfg(unix)]
fn os_str_component_bytes(component: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;

    Cow::Borrowed(component.as_bytes())
}

#[cfg(not(unix))]
fn os_str_component_bytes(component: &std::ffi::OsStr) -> Cow<'_, [u8]> {
    Cow::Owned(component.to_string_lossy().into_owned().into_bytes())
}

fn collect_worktree_entries(
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
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
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
            if metadata.is_dir()
                && context.tracked_paths.is_some_and(|tracked_paths| {
                    tracked_paths_may_contain(tracked_paths, &git_path)
                })
            {
                collect_worktree_entries(context, &path, &git_path)?;
            }
            continue;
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
                let dirt = submodule_dirt(&path);
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
            if is_nested_repository_boundary(&path) {
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
                if context.record_clean_tracked {
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
                apply_clean_filter_with_attributes(context.config, &checks, &git_path, &body)?
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

fn tracked_paths_may_contain(tracked_paths: &BTreeSet<Vec<u8>>, directory: &[u8]) -> bool {
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

fn is_same_path(left: &Path, right: &Path) -> bool {
    left == right
}

/// Whether `path`'s final component is `.git`. Git never lists a `.git` entry at
/// any depth (a repository's own `.git`, a submodule gitlink file, or an embedded
/// repository's `.git` directory) as untracked content.
fn is_dot_git_entry(path: &Path) -> bool {
    path.file_name() == Some(std::ffi::OsStr::new(".git"))
}

/// Whether `path` is a directory containing an embedded repository's `.git`
/// *directory*, or a `.git` file whose `gitdir:` pointer resolves to an
/// existing directory (a submodule worktree). Git treats both as a repository
/// boundary (listing the directory as `dir/`); an *invalid* `.git` file (no
/// resolvable `gitdir:` target) is not a boundary — Git descends into the
/// directory and lists its other untracked contents normally.
fn is_nested_repository_boundary(path: &Path) -> bool {
    if path.join(".git").is_dir() {
        return true;
    }
    sley_diff_merge::gitlink_git_dir(path).is_some()
}

/// Whether `path` is an embedded repository's `.git` directory or a path inside it.
fn is_embedded_git_internals(root: &Path, path: &Path) -> bool {
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

fn worktree_entry_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.file_type().is_symlink() {
        0o120000
    } else if metadata.is_dir() {
        0o040000
    } else {
        file_mode(metadata)
    }
}

fn worktree_path(root: &Path, path: &[u8]) -> Result<PathBuf> {
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

fn remove_worktree_file(root: &Path, path: &[u8]) -> Result<()> {
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

fn prune_empty_parents(root: &Path, mut dir: Option<&Path>) -> Result<()> {
    while let Some(path) = dir {
        if path == root {
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

fn git_tree_entry_cmp(
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

fn tree_name_terminator(mode: u32) -> u8 {
    if mode == 0o040000 { b'/' } else { 0 }
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

/// The blob content git stores for a symlink: the raw bytes of the link target
/// exactly as `readlink(2)` returns them. On Unix the target is an opaque byte
/// string, so we take the `OsStr` bytes verbatim (no UTF-8 round-trip, no path
/// re-componentization that could rewrite separators).
#[cfg(unix)]
fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let target = fs::read_link(path)?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn symlink_target_bytes(path: &Path) -> Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    // git normalizes symlink targets to forward slashes on platforms whose
    // native separator is `\`.
    Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
}

fn git_path_bytes(path: &Path) -> Result<Vec<u8>> {
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

fn repo_path_to_os_path(path: &[u8]) -> Result<PathBuf> {
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

fn git_path_to_relative_path(path: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(path)
        .map_err(|err| GitError::InvalidPath(format!("invalid utf-8 index path: {err}")))?;
    Ok(path.split('/').collect())
}

fn path_has_trailing_separator(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_odb::ObjectReader;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn short_status(
        worktree_root: impl AsRef<Path>,
        git_dir: impl AsRef<Path>,
        format: ObjectFormat,
    ) -> Result<Vec<ShortStatusEntry>> {
        let mut entries = Vec::new();
        stream_short_status(worktree_root, git_dir, format, |entry| {
            entries.push(entry.to_owned_entry());
            Ok(StreamControl::Continue)
        })?;
        Ok(entries)
    }

    #[test]
    fn atomic_metadata_writer_writes_and_reports_stat() {
        let root = temp_root();
        let path = root.join(".git").join("HEAD");

        let result = write_metadata_file_atomic(
            &path,
            b"ref: refs/heads/main\n",
            AtomicMetadataWriteOptions::default(),
        )
        .expect("write metadata");

        assert_eq!(
            fs::read(&path).expect("read metadata"),
            b"ref: refs/heads/main\n"
        );
        assert_eq!(result.path, path);
        assert_eq!(result.len, b"ref: refs/heads/main\n".len() as u64);
        assert!(result.mtime.is_some());
        assert!(!path.with_file_name("HEAD.lock").exists());
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn atomic_metadata_writer_existing_lock_preserves_original() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("create git dir");
        let path = git_dir.join("HEAD");
        let lock = git_dir.join("HEAD.lock");
        fs::write(&path, b"ref: refs/heads/main\n").expect("write original");
        fs::write(&lock, b"held\n").expect("write lock");

        let err = write_metadata_file_atomic(
            &path,
            b"ref: refs/heads/other\n",
            AtomicMetadataWriteOptions::default(),
        )
        .expect_err("held lock must fail");

        assert!(matches!(err, GitError::Transaction(_)));
        assert_eq!(
            fs::read(&path).expect("read original"),
            b"ref: refs/heads/main\n"
        );
        assert_eq!(fs::read(&lock).expect("read lock"), b"held\n");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // --- `ls-files --eol` stat/attr helpers (mirror convert.c) ---------------

    #[test]
    fn convert_stats_ascii_classifies_eol_content() {
        assert_eq!(convert_stats_ascii(b""), "none");
        assert_eq!(convert_stats_ascii(b"abc"), "none");
        assert_eq!(convert_stats_ascii(b"a\nb\n"), "lf");
        assert_eq!(convert_stats_ascii(b"a\r\nb\r\n"), "crlf");
        assert_eq!(convert_stats_ascii(b"a\r\nb\n"), "mixed");
        // A lone CR makes the content binary (-text), matching git.
        assert_eq!(convert_stats_ascii(b"a\rb"), "-text");
        // A NUL byte is binary.
        assert_eq!(convert_stats_ascii(b"a\0b\n"), "-text");
        // A trailing ^Z (EOF) is not counted as non-printable.
        assert_eq!(convert_stats_ascii(b"abc\n\x1a"), "lf");
    }

    fn attr_check(name: &[u8], state: Option<AttributeState>) -> AttributeCheck {
        AttributeCheck {
            attribute: name.to_vec(),
            state,
        }
    }

    #[test]
    fn convert_attr_ascii_matches_git_attr_action() {
        // No attributes at all: empty attr field.
        assert_eq!(convert_attr_ascii(&[]), "");
        // text (set) -> "text"; -text (unset) -> "-text".
        assert_eq!(
            convert_attr_ascii(&[attr_check(b"text", Some(AttributeState::Set))]),
            "text"
        );
        assert_eq!(
            convert_attr_ascii(&[attr_check(b"text", Some(AttributeState::Unset))]),
            "-text"
        );
        // text=auto -> "text=auto"; with eol=crlf/lf the AUTO variants.
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"text",
                Some(AttributeState::Value(b"auto".to_vec()))
            )]),
            "text=auto"
        );
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Value(b"auto".to_vec()))),
                attr_check(b"eol", Some(AttributeState::Value(b"crlf".to_vec()))),
            ]),
            "text=auto eol=crlf"
        );
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Value(b"auto".to_vec()))),
                attr_check(b"eol", Some(AttributeState::Value(b"lf".to_vec()))),
            ]),
            "text=auto eol=lf"
        );
        // eol=crlf/lf alone (no text) forces text + the eol direction.
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"eol",
                Some(AttributeState::Value(b"crlf".to_vec()))
            )]),
            "text eol=crlf"
        );
        assert_eq!(
            convert_attr_ascii(&[attr_check(
                b"eol",
                Some(AttributeState::Value(b"lf".to_vec()))
            )]),
            "text eol=lf"
        );
        // -text overrides any eol attribute (binary wins).
        assert_eq!(
            convert_attr_ascii(&[
                attr_check(b"text", Some(AttributeState::Unset)),
                attr_check(b"eol", Some(AttributeState::Value(b"crlf".to_vec()))),
            ]),
            "-text"
        );
    }

    #[test]
    fn smudge_safety_guard_skips_irreversible_autocrlf() {
        // text=auto eol=crlf (AUTO_CRLF): convert pure-LF, but leave content
        // alone when it already has a CR or CRLF, or is binary.
        let auto = ContentFilterPlan {
            text: TextDecision::Auto,
            eol: EolConversion::Crlf,
            driver: None,
        };
        assert!(auto.will_convert_lf_to_crlf(b"a\nb\n"));
        assert!(!auto.will_convert_lf_to_crlf(b"a\r\nb\n")); // has CRLF
        assert!(!auto.will_convert_lf_to_crlf(b"a\nb\rc")); // lone CR (binary)
        assert!(!auto.will_convert_lf_to_crlf(b"abc")); // no naked LF

        // text eol=crlf (TEXT_CRLF): no safety guard — always convert naked LF
        // even when a CR/CRLF is already present.
        let text = ContentFilterPlan {
            text: TextDecision::Text,
            eol: EolConversion::Crlf,
            driver: None,
        };
        assert!(text.will_convert_lf_to_crlf(b"a\r\nb\nc\n"));
        assert!(!text.will_convert_lf_to_crlf(b"a\r\nb\r\n")); // no naked LF
    }

    /// Build an in-memory ignore matcher from raw `.gitignore` lines (no disk).
    fn ignore_matcher(patterns: &[&[u8]]) -> IgnoreMatcher {
        let mut matcher = IgnoreMatcher::default();
        let owned: Vec<Vec<u8>> = patterns.iter().map(|p| p.to_vec()).collect();
        matcher.extend_patterns(&owned);
        matcher
    }

    #[test]
    fn ignore_match_kind_fast_paths_match_the_wildcard_engine() {
        // Literal: exact basename anywhere; not a superstring.
        let matcher = ignore_matcher(&[b"Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", false));
        assert!(!matcher.is_ignored(b"Pods_not", false));
        assert!(matches!(
            classify_ignore_pattern(b"Pods"),
            MatchKind::Literal
        ));

        // Suffix `*.log`: basename ending in `.log` at any depth.
        let matcher = ignore_matcher(&[b"*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(matcher.is_ignored(b"a/b/x.log", false));
        assert!(matcher.is_ignored(b".log", false));
        assert!(!matcher.is_ignored(b"x.logx", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.log"),
            MatchKind::Suffix
        ));

        // Prefix `build*`: basename starting with `build`.
        let matcher = ignore_matcher(&[b"build*"]);
        assert!(matcher.is_ignored(b"buildfoo", false));
        assert!(matcher.is_ignored(b"a/build", false));
        assert!(!matcher.is_ignored(b"xbuild", false));
        assert!(matches!(
            classify_ignore_pattern(b"build*"),
            MatchKind::Prefix
        ));
    }

    #[test]
    fn ignore_anchored_suffix_does_not_cross_slash() {
        // `/*.log` is anchored: matches `.log` files only at the matcher base,
        // never in a subdirectory — the slash guard in `match_segment`.
        let matcher = ignore_matcher(&[b"/*.log"]);
        assert!(matcher.is_ignored(b"x.log", false));
        assert!(!matcher.is_ignored(b"sub/x.log", false));

        // Anchored literal likewise only matches at root.
        let matcher = ignore_matcher(&[b"/foo"]);
        assert!(matcher.is_ignored(b"foo", false));
        assert!(!matcher.is_ignored(b"a/foo", false));
    }

    #[test]
    fn ignore_anchored_directory_glob_matches_root_directory() {
        let matcher = ignore_matcher(&[b"/tmp-*/"]);
        assert!(matcher.is_ignored(b"tmp-info-only", true));
        assert!(matcher.is_ignored(b"tmp-info-only/file.txt", false));
        assert!(!matcher.is_ignored(b"nested/tmp-info-only", true));
        assert!(!matcher.is_ignored(b"tmp-info-only", false));
    }

    #[test]
    fn ignore_negated_directory_glob_does_not_reinclude_files() {
        // t0008-ignores "directories and ** matches": a negated directory-only
        // pattern re-includes *directories* but never the *files* inside them
        // (git: re-including a dir with `!dir/` still needs an explicit
        // `!dir/*` to reach its files). Verified against git 2.54 check-ignore:
        //   data/file              -> data/**           (ignored)
        //   data/data1/file1       -> data/**           (ignored, NOT !data/**/)
        //   data/data1/file1.txt   -> !data/**/*.txt    (re-included)
        //   data/data1   (dir)     -> !data/**/         (re-included)
        let matcher = ignore_matcher(&[b"data/**", b"!data/**/", b"!data/**/*.txt"]);
        // Files stay ignored: `!data/**/` must not win the file leaf scan.
        assert!(matcher.is_ignored(b"data/file", false));
        assert!(matcher.is_ignored(b"data/data1/file1", false));
        assert!(matcher.is_ignored(b"data/data2/file2", false));
        // `.txt` files are re-included by the explicit non-dir negation.
        assert!(!matcher.is_ignored(b"data/data1/file1.txt", false));
        assert!(!matcher.is_ignored(b"data/data2/file2.txt", false));
        // Directories ARE re-included by `!data/**/` (the directory-glob gain
        // from `fix: match git status ignored directory globs`).
        assert!(!matcher.is_ignored(b"data/data1", true));
        assert!(!matcher.is_ignored(b"data/data2", true));
    }

    #[test]
    fn ignore_double_star_prefix_collapses_to_basename() {
        // `**/X` ≡ `X` for slash-free X (verified against `git check-ignore`).
        let matcher = ignore_matcher(&[b"**/Pods"]);
        assert!(matcher.is_ignored(b"a/b/Pods", true));
        assert!(matcher.is_ignored(b"Pods", true));
        assert!(!matcher.is_ignored(b"Pods_not", false));

        let matcher = ignore_matcher(&[b"**/*.jks"]);
        assert!(matcher.is_ignored(b"x.jks", false));
        assert!(matcher.is_ignored(b"a/deep/y.jks", false));
        assert!(!matcher.is_ignored(b"x.jksx", false));

        // `**/A/B` keeps a slash in the tail, so it stays a real glob and must
        // match the trailing path at any depth.
        let matcher = ignore_matcher(&[b"**/Flutter/ephemeral"]);
        assert!(matcher.is_ignored(b"Flutter/ephemeral", true));
        assert!(matcher.is_ignored(b"a/Flutter/ephemeral", true));
        assert!(!matcher.is_ignored(b"Flutter/other", true));
        assert!(matches!(
            classify_ignore_pattern(b"**/Flutter/ephemeral"),
            MatchKind::PathSuffix
        ));
    }

    #[test]
    fn ignore_slash_glob_literal_basename_bucket_preserves_matches() {
        let matcher = ignore_matcher(&[b"**/android/**/GeneratedPluginRegistrant.java"]);
        assert!(
            matcher
                .buckets
                .glob_path_literal_basename
                .contains_key(b"GeneratedPluginRegistrant.java".as_slice())
        );
        assert!(matcher.is_ignored(
            b"packages/app/android/src/GeneratedPluginRegistrant.java",
            false
        ));
        assert!(matcher.is_ignored(
            b"android/app/src/main/java/io/flutter/GeneratedPluginRegistrant.java",
            false
        ));
        assert!(!matcher.is_ignored(b"android/app/src/main/java/io/flutter/Other.java", false));

        let matcher = ignore_matcher(&[b"**/ios/**/Pods/"]);
        assert!(
            matcher
                .buckets
                .glob_directory_literal_basename
                .contains_key(b"Pods".as_slice())
        );
        assert!(matcher.is_ignored(b"ios/Runner/Pods", true));
        assert!(matcher.is_ignored(b"dev/app/ios/Runner/Pods/Manifest.lock", false));
        assert!(!matcher.is_ignored(b"dev/app/ios/Runner/Podfile", false));

        let matcher = ignore_matcher(&[b"**/ios/**/*.mode1v3"]);
        assert!(
            !matcher.buckets.glob_path_suffix_basename.is_empty(),
            "suffix-final slash glob should be prefiltered by basename suffix"
        );
        assert!(matcher.is_ignored(b"apps/ios/Runner/default.mode1v3", false));
        assert!(!matcher.is_ignored(b"apps/ios/Runner/default.mode2v3", false));

        let matcher = ignore_matcher(&[b"**/ios/Runner/GeneratedPluginRegistrant.*"]);
        assert!(
            !matcher.buckets.glob_path_prefix_basename.is_empty(),
            "prefix-final slash glob should be prefiltered by basename prefix"
        );
        assert!(matcher.is_ignored(b"apps/ios/Runner/GeneratedPluginRegistrant.swift", false));
        assert!(!matcher.is_ignored(
            b"apps/ios/Runner/OtherGeneratedPluginRegistrant.swift",
            false
        ));

        let matcher = ignore_matcher(&[b"ios/Scenarios/*.framework/"]);
        assert!(
            !matcher.buckets.glob_directory_suffix_basename.is_empty(),
            "directory suffix-final slash glob should be prefiltered by directory component"
        );
        assert!(matcher.is_ignored(b"ios/Scenarios/App.framework", true));
        assert!(matcher.is_ignored(b"ios/Scenarios/App.framework/Info.plist", false));
        assert!(!matcher.is_ignored(b"ios/Scenarios/App.xcframework/Info.plist", false));
    }

    #[test]
    fn ignore_complex_globs_still_use_the_engine() {
        let matcher = ignore_matcher(&[b"*.[Cc]ache"]);
        assert!(matcher.is_ignored(b"x.cache", false));
        assert!(matcher.is_ignored(b"x.Cache", false));
        assert!(!matcher.is_ignored(b"x.xache", false));
        assert!(matches!(
            classify_ignore_pattern(b"*.[Cc]ache"),
            MatchKind::Glob
        ));

        let matcher = ignore_matcher(&[b"Icon?"]);
        assert!(matcher.is_ignored(b"IconA", false));
        assert!(!matcher.is_ignored(b"Icon", false));
        assert!(!matcher.is_ignored(b"IconAB", false));

        // Multi-star is not a simple prefix/suffix.
        assert!(matches!(
            classify_ignore_pattern(b"app.*.symbols"),
            MatchKind::Glob
        ));
        assert!(matches!(classify_ignore_pattern(b"a*b*c"), MatchKind::Glob));

        let matcher = ignore_matcher(&[b".vscode/*", b"dev/devicelab/ABresults*.json"]);
        assert!(matcher.is_ignored(b".vscode/settings.json", false));
        assert!(!matcher.is_ignored(b"pkg/.vscode/settings.json", false));
        assert!(matcher.is_ignored(b"dev/devicelab/ABresults-1.json", false));
        assert!(!matcher.is_ignored(b"dev/devicelab/results-1.json", false));
    }

    #[test]
    fn ignore_negation_still_applies_after_fast_paths() {
        // Last match wins: a negated literal un-ignores a suffix-matched file.
        let matcher = ignore_matcher(&[b"*.log", b"!keep.log"]);
        assert!(matcher.is_ignored(b"a/x.log", false));
        assert!(!matcher.is_ignored(b"a/keep.log", false));
    }

    #[test]
    fn read_expected_object_missing_blob_exposes_oid_and_kind() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let db = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let missing = ObjectId::empty_blob(ObjectFormat::Sha1);

        let err = read_expected_object(&db, &missing, ObjectType::Blob)
            .expect_err("missing blob should error");
        let kind = err.not_found_kind().expect("typed not found");
        assert_eq!(kind.object_id(), Some(missing));
        assert_eq!(kind.missing_object_kind(), Some(MissingObjectKind::Blob));
        assert_eq!(
            kind.missing_object_context(),
            Some(MissingObjectContext::WorktreeMaterialize)
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn update_index_adds_file_entry_and_blob() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);
        let index = Index::parse_v2_sha1(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn update_index_and_write_tree_support_sha256() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha256,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 1);

        let index = Index::parse(
            &fs::read(repository_index_path(&git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha256,
        )
        .expect("test operation should succeed");
        assert_eq!(index.entries[0].path, b"hello.txt");
        assert_eq!(index.entries[0].oid.format(), ObjectFormat::Sha256);

        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha256)
            .expect("test operation should succeed");
        assert_eq!(tree_oid.format(), ObjectFormat::Sha256);
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha256);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn write_tree_from_index_writes_nested_tree_objects() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src")).expect("test operation should succeed");
        fs::write(root.join("README.md"), b"readme\n").expect("test operation should succeed");
        fs::write(root.join("src").join("lib.rs"), b"pub fn demo() {}\n")
            .expect("test operation should succeed");
        let result = add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("README.md"), PathBuf::from("src/lib.rs")],
        )
        .expect("test operation should succeed");
        assert_eq!(result.entries, 2);
        let tree_oid = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let tree = odb
            .read_object(&tree_oid)
            .expect("test operation should succeed");
        assert_eq!(tree.object_type, ObjectType::Tree);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_reports_added_and_untracked_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("hello.txt"), b"hello\n").expect("test operation should succeed");
        fs::write(root.join("extra.txt"), b"extra\n").expect("test operation should succeed");
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("hello.txt")],
        )
        .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec!["A  hello.txt", "?? extra.txt"]
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_root_is_none_for_bare_repository() {
        // A bare git_dir (basename `.git`) with `core.bare = true` must resolve to
        // `Ok(None)` rather than falling through to the "parent of .git" case.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("create bare git dir");
        // Hermetic minimal config — do not depend on host gitconfig.
        fs::write(git_dir.join("config"), b"[core]\n\tbare = true\n").expect("write bare config");

        assert_eq!(
            worktree_root_for_git_dir(&git_dir).expect("resolve bare worktree root"),
            None,
            "a bare repository has no working tree"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_root_is_parent_for_non_bare_dot_git() {
        // A non-bare `.git` directory (no core.bare / core.bare = false) still
        // resolves to its parent — the ordinary non-bare layout.
        let root = temp_root();
        let work = root.join("work");
        let git_dir = work.join(".git");
        fs::create_dir_all(&git_dir).expect("create non-bare git dir");
        fs::write(git_dir.join("config"), b"[core]\n\tbare = false\n")
            .expect("write non-bare config");

        assert_eq!(
            worktree_root_for_git_dir(&git_dir).expect("resolve non-bare worktree root"),
            Some(work.clone()),
            "a non-bare .git dir resolves to its parent"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-worktree-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test operation should succeed");
        path
    }

    fn index_entry_for<'a>(index: &'a Index, path: &[u8]) -> &'a IndexEntry {
        index
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("missing index entry for {}", String::from_utf8_lossy(path)))
    }

    fn read_index(git_dir: &Path) -> Index {
        Index::parse(
            &fs::read(repository_index_path(git_dir)).expect("test operation should succeed"),
            ObjectFormat::Sha1,
        )
        .expect("test operation should succeed")
    }

    /// Stages `paths` from the worktree, writes their tree, wraps it in a commit
    /// object, and points `refs/heads/main` + `HEAD` at it. Returns the commit
    /// id. After this call the index reflects the committed tree.
    fn build_commit(root: &Path, git_dir: &Path, paths: &[&str]) -> ObjectId {
        let path_bufs = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        add_paths_to_index(root, git_dir, ObjectFormat::Sha1, &path_bufs)
            .expect("test operation should succeed");
        let tree = write_tree_from_index(git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"committer Test <test@example.com> 0 +0000\n");
        body.extend_from_slice(b"\n");
        body.extend_from_slice(b"sparse fixture\n");
        let odb = FileObjectDatabase::from_git_dir(git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "refs/heads/main".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Symbolic("refs/heads/main".into()),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");
        commit
    }

    fn full_sparse(patterns: &[&[u8]]) -> SparseCheckout {
        SparseCheckout {
            patterns: patterns.iter().map(|pattern| pattern.to_vec()).collect(),
            sparse_index: false,
        }
    }

    #[test]
    fn apply_sparse_checkout_full_mode_skips_out_of_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        fs::write(root.join("top.txt"), b"top\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt", "top.txt"]);

        // Full (non-cone) pattern: keep only the `in/` subtree.
        let sparse = full_sparse(&[b"/in/"]);
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");

        assert!(root.join("in").join("keep.txt").exists());
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(!root.join("top.txt").exists());
        assert!(result.materialized.contains(&b"in/keep.txt".to_vec()));
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        assert!(result.skipped.contains(&b"top.txt".to_vec()));

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"in/keep.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index, b"top.txt"
        )));
        // Out-of-cone entries are preserved in the index, just not on disk.
        assert_eq!(index.entries.len(), 3);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_toggle_rematerializes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("a")).expect("test operation should succeed");
        fs::create_dir_all(root.join("b")).expect("test operation should succeed");
        fs::write(root.join("a").join("file.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("b").join("file.txt"), b"b\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["a/file.txt", "b/file.txt"]);

        // First narrow to `a/`.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/a/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(root.join("a").join("file.txt").exists());
        assert!(!root.join("b").join("file.txt").exists());
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));

        // Now switch the cone to `b/`: `a/` must leave, `b/` must come back with
        // the correct content, and the skip-worktree bits must flip.
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &full_sparse(&[b"/b/"]),
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("a").join("file.txt").exists());
        assert!(root.join("b").join("file.txt").exists());
        assert_eq!(
            fs::read(root.join("b").join("file.txt")).expect("test operation should succeed"),
            b"b\n"
        );
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"a/file.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"b/file.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_cone_mode_matches_directory_prefixes() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("kept").join("nested"))
            .expect("test operation should succeed");
        fs::create_dir_all(root.join("other")).expect("test operation should succeed");
        fs::write(root.join("kept").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("kept").join("nested").join("b.txt"), b"b\n")
            .expect("test operation should succeed");
        fs::write(root.join("other").join("c.txt"), b"c\n").expect("test operation should succeed");
        fs::write(root.join("root.txt"), b"r\n").expect("test operation should succeed");
        build_commit(
            &root,
            &git_dir,
            &["kept/a.txt", "kept/nested/b.txt", "other/c.txt", "root.txt"],
        );

        // Standard cone patterns: top-level files plus the whole `kept/` tree.
        let sparse = SparseCheckout {
            patterns: vec![b"/*".to_vec(), b"!/*/".to_vec(), b"/kept/".to_vec()],
            sparse_index: false,
        };
        // Auto mode should detect cone shape on its own.
        assert!(patterns_are_cone(&sparse.patterns));
        apply_sparse_checkout(&root, &git_dir, ObjectFormat::Sha1, &sparse)
            .expect("test operation should succeed");

        assert!(root.join("root.txt").exists());
        assert!(root.join("kept").join("a.txt").exists());
        assert!(root.join("kept").join("nested").join("b.txt").exists());
        assert!(!root.join("other").join("c.txt").exists());

        let index = read_index(&git_dir);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"root.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/a.txt"
        )));
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"kept/nested/b.txt"
        )));
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"other/c.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn apply_sparse_checkout_honors_preexisting_skip_worktree_via_idempotence() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("in")).expect("test operation should succeed");
        fs::create_dir_all(root.join("out")).expect("test operation should succeed");
        fs::write(root.join("in").join("keep.txt"), b"keep\n")
            .expect("test operation should succeed");
        fs::write(root.join("out").join("drop.txt"), b"drop\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["in/keep.txt", "out/drop.txt"]);

        let sparse = full_sparse(&[b"/in/"]);
        apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());

        // Re-applying the same spec is a no-op: the already-skipped file stays
        // absent and the bit stays set (we do not resurrect it).
        let result = apply_sparse_checkout_with_mode(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &sparse,
            SparseCheckoutMode::Full,
        )
        .expect("test operation should succeed");
        assert!(!root.join("out").join("drop.txt").exists());
        assert!(root.join("in").join("keep.txt").exists());
        assert!(result.skipped.contains(&b"out/drop.txt".to_vec()));
        let index = read_index(&git_dir);
        assert!(index_entry_skip_worktree(index_entry_for(
            &index,
            b"out/drop.txt"
        )));
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn checkout_detached_sparse_only_writes_in_cone_paths() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("keep")).expect("test operation should succeed");
        fs::create_dir_all(root.join("skip")).expect("test operation should succeed");
        fs::write(root.join("keep").join("a.txt"), b"a\n").expect("test operation should succeed");
        fs::write(root.join("skip").join("b.txt"), b"b\n").expect("test operation should succeed");
        let commit = build_commit(&root, &git_dir, &["keep/a.txt", "skip/b.txt"]);

        // The worktree is clean and matches the commit. A sparse checkout must
        // keep the in-cone file and evict the out-of-cone one.
        let sparse = full_sparse(&[b"/keep/"]);
        let result = checkout_detached_sparse(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"Test <test@example.com> 0 +0000".to_vec(),
            b"checkout".to_vec(),
            &sparse,
        )
        .expect("test operation should succeed");
        assert_eq!(result.files, 2);

        assert!(root.join("keep").join("a.txt").exists());
        assert_eq!(
            fs::read(root.join("keep").join("a.txt")).expect("test operation should succeed"),
            b"a\n"
        );
        assert!(!root.join("skip").join("b.txt").exists());

        let index = read_index(&git_dir);
        assert_eq!(index.entries.len(), 2);
        assert!(!index_entry_skip_worktree(index_entry_for(
            &index,
            b"keep/a.txt"
        )));
        let skipped = index_entry_for(&index, b"skip/b.txt");
        assert!(index_entry_skip_worktree(skipped));
        // The skipped entry still carries the committed blob id and mode.
        assert_eq!(skipped.mode, 0o100644);
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    // ----- content filtering: EOL / autocrlf + clean/smudge drivers -----

    /// Build a [`GitConfig`] from raw config text.
    fn config_from(text: &str) -> GitConfig {
        GitConfig::parse(text.as_bytes()).expect("test operation should succeed")
    }

    /// Conformance grid for git's `output_eol(crlf_action)` decision table
    /// (convert.c) on the smudge side, exercised across the same
    /// attr × autocrlf × eol × content matrix as upstream t0027/t0026.
    ///
    /// Each row asserts the smudge output for a representative content shape.
    /// The cases that historically under-converted are the non-`auto` `text`
    /// paths (the auto-only safety guard must NOT fire) and the
    /// `autocrlf=true overrides core.eol` precedence rows.
    #[test]
    fn smudge_output_eol_decision_table() {
        // Naked-LF-only blob (the canonical "should gain CRLF" case).
        const LF: &[u8] = b"a\nb\nc\n";
        // Mixed CRLF + naked LF: a non-auto crlf action converts the naked LFs
        // to CRLF (whole file becomes CRLF); an auto action leaves it untouched.
        const CRLF_MIX_LF: &[u8] = b"a\r\nb\nc\r\n";
        // Naked LF plus a lone CR: non-auto converts LFs, keeping the lone CR.
        const LF_MIX_CR: &[u8] = b"a\nb\rc\n";

        let smudge = |cfg: &str, attrline: Option<&[u8]>, input: &[u8]| -> Vec<u8> {
            let config = config_from(cfg);
            let checks = match attrline {
                Some(line) => {
                    let mut matcher = AttributeMatcher::default();
                    read_attribute_patterns_from_bytes(line, &mut matcher, &[]);
                    matcher.attributes_for_path(b"f.txt", &filter_attribute_names(), false)
                }
                None => Vec::new(),
            };
            apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", input)
                .expect("smudge must succeed")
        };

        // --- attr=text (CRLF_TEXT_*): non-auto, the safety guard must not fire.
        // text + eol=crlf => CRLF_TEXT_CRLF: every naked LF gains CR.
        let attr_text_crlf: &[u8] = b"*.txt text eol=crlf";
        for cfg in [
            "[core]\n\tautocrlf = false\n\teol = lf\n",
            "[core]\n\tautocrlf = false\n\teol = crlf\n",
            "[core]\n\tautocrlf = true\n\teol = lf\n",
            "[core]\n\tautocrlf = input\n",
        ] {
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), LF),
                b"a\r\nb\r\nc\r\n",
                "text eol=crlf must add CR to naked LF (cfg={cfg:?})"
            );
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), CRLF_MIX_LF),
                b"a\r\nb\r\nc\r\n",
                "text eol=crlf must convert mixed content fully (cfg={cfg:?})"
            );
            assert_eq!(
                smudge(cfg, Some(attr_text_crlf), LF_MIX_CR),
                b"a\r\nb\rc\r\n",
                "text eol=crlf keeps the lone CR but adds CR to naked LF (cfg={cfg:?})"
            );
        }

        // --- attr=text, no eol attr: CRLF_TEXT, resolved by text_eol_is_crlf().
        // autocrlf=true wins over core.eol=lf (the precedence fix).
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n\teol = lf\n",
                Some(b"*.txt text"),
                LF
            ),
            b"a\r\nb\r\nc\r\n",
            "autocrlf=true must override core.eol=lf for plain text attr"
        );
        // autocrlf unset, core.eol=crlf => CRLF.
        assert_eq!(
            smudge("[core]\n\teol = crlf\n", Some(b"*.txt text"), LF),
            b"a\r\nb\r\nc\r\n",
            "core.eol=crlf adds CR to naked LF for plain text attr"
        );
        // autocrlf unset, core.eol=lf (and native LF on this host) => no CR.
        assert_eq!(
            smudge("[core]\n\teol = lf\n", Some(b"*.txt text"), LF),
            LF,
            "core.eol=lf leaves naked LF untouched on smudge"
        );
        // text + autocrlf=input => CRLF_TEXT_INPUT: no CR on smudge.
        assert_eq!(
            smudge("[core]\n\tautocrlf = input\n", Some(b"*.txt text"), LF),
            LF,
            "autocrlf=input overrides core.eol; no CR on smudge"
        );

        // --- attr=text=auto (CRLF_AUTO_*): the safety guard DOES fire.
        // auto + autocrlf=true + naked-LF-only => convert.
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n", Some(b"*.txt text=auto"), LF),
            b"a\r\nb\r\nc\r\n",
            "text=auto converts a clean naked-LF file"
        );
        // auto + already has a CR/CRLF => leave untouched (irreversible guard).
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n",
                Some(b"*.txt text=auto"),
                CRLF_MIX_LF
            ),
            CRLF_MIX_LF,
            "text=auto must not touch content that already has CRLF"
        );
        assert_eq!(
            smudge(
                "[core]\n\tautocrlf = true\n",
                Some(b"*.txt text=auto"),
                LF_MIX_CR
            ),
            LF_MIX_CR,
            "text=auto must not touch content that already has a lone CR"
        );

        // --- no attr, autocrlf=true => CRLF_AUTO_CRLF (auto guard applies).
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n\teol = lf\n", None, LF),
            b"a\r\nb\r\nc\r\n",
            "autocrlf=true (no attr) converts clean naked-LF and overrides core.eol=lf"
        );
        // --- no attr, autocrlf=false => CRLF_BINARY: never convert.
        assert_eq!(
            smudge("[core]\n\teol = crlf\n", None, LF),
            LF,
            "no attr + autocrlf=false leaves content untouched even with core.eol=crlf"
        );
        // --- -text (CRLF_BINARY): never convert regardless of config.
        assert_eq!(
            smudge("[core]\n\tautocrlf = true\n", Some(b"*.txt -text"), LF),
            LF,
            "-text is binary: never convert"
        );
    }

    /// Resolve attribute checks against an on-disk `.gitattributes` in `root`.
    fn attrs(root: &Path, path: &[u8]) -> Vec<AttributeCheck> {
        filter_attribute_checks(root, path).expect("test operation should succeed")
    }

    #[test]
    fn standard_attribute_matcher_matches_per_path_lookup() {
        let root = temp_root();
        fs::create_dir_all(root.join(".git").join("info")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src").join("nested")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.rs diff=rust\n")
            .expect("test operation should succeed");
        fs::write(
            root.join("src").join(".gitattributes"),
            b"*.rs diff=python\n",
        )
        .expect("test operation should succeed");
        fs::write(
            root.join(".git").join("info").join("attributes"),
            b"src/nested/*.rs diff=java\n",
        )
        .expect("test operation should succeed");

        let requested = vec![b"diff".to_vec()];
        let path = b"src/nested/file.rs";
        let per_path = standard_attributes_for_path(&root, path, &requested, false)
            .expect("test operation should succeed");
        let matcher = StandardAttributeMatcher::from_worktree_root(&root)
            .expect("test operation should succeed");
        assert_eq!(
            matcher.attributes_for_path(path, &requested, false),
            per_path
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn filter_attribute_lookup_reads_only_path_chain() {
        let root = temp_root();
        fs::create_dir_all(root.join(".git").join("info")).expect("test operation should succeed");
        fs::create_dir_all(root.join("src").join("nested")).expect("test operation should succeed");
        fs::create_dir_all(root.join("sibling")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.txt text\n")
            .expect("test operation should succeed");
        fs::write(root.join("src").join(".gitattributes"), b"*.txt -text\n")
            .expect("test operation should succeed");
        fs::write(
            root.join("sibling").join(".gitattributes"),
            b"*.txt eol=crlf\n",
        )
        .expect("test operation should succeed");
        fs::write(
            root.join(".git").join("info").join("attributes"),
            b"src/nested/*.txt eol=lf\n",
        )
        .expect("test operation should succeed");

        let path = b"src/nested/file.txt";
        let full = standard_attributes_for_path(&root, path, &filter_attribute_names(), false)
            .expect("test operation should succeed");
        assert_eq!(filter_attribute_checks(&root, path).unwrap(), full);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn crlf_to_lf_collapses_only_pairs() {
        assert_eq!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\r\nb\r\n")).as_ref(),
            b"a\nb\n"
        );
        // A lone CR (no following LF) is preserved.
        assert_eq!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\rb")).as_ref(),
            b"a\rb"
        );
        // An already-LF stream is unchanged.
        assert!(matches!(
            convert_crlf_to_lf_cow(Cow::Borrowed(b"a\nb\n")),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn lf_to_crlf_does_not_double_convert() {
        assert_eq!(convert_lf_to_crlf(b"a\nb\n"), b"a\r\nb\r\n");
        // Existing CRLF is left intact (no extra CR added).
        assert_eq!(convert_lf_to_crlf(b"a\r\nb\r\n"), b"a\r\nb\r\n");
    }

    #[test]
    fn autocrlf_round_trip_clean_then_smudge() {
        // autocrlf=true: worktree CRLF -> blob LF on clean, blob LF -> worktree
        // CRLF on smudge.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let worktree = b"line1\r\nline2\r\n";
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", worktree)
            .expect("test operation should succeed");
        assert_eq!(blob, b"line1\nline2\n", "clean must normalize CRLF to LF");
        let restored = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            restored, worktree,
            "smudge must restore CRLF from the LF blob"
        );
    }

    #[test]
    fn conv_flags_from_config_matches_git_defaults() {
        // Unset core.safecrlf defaults to WARN (git's global_conv_flags_eol).
        assert_eq!(ConvFlags::from_config(&config_from("")), ConvFlags::Warn);
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = warn\n")),
            ConvFlags::Warn
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = WARN\n")),
            ConvFlags::Warn
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = true\n")),
            ConvFlags::Die
        );
        assert_eq!(
            ConvFlags::from_config(&config_from("[core]\n\tsafecrlf = false\n")),
            ConvFlags::Off
        );
    }

    #[test]
    fn safecrlf_warn_does_not_change_clean_bytes() {
        // The warning is purely additive: byte output is identical whether
        // safecrlf is off or warn.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let worktree = b"a\nb\nc\n";
        let plain = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", worktree)
            .expect("clean");
        let warned = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            worktree,
            ConvFlags::Warn,
            SafeCrlfIndexBlob::None,
        )
        .expect("clean with safecrlf")
        .into_owned();
        assert_eq!(plain, warned, "safecrlf must not alter the cleaned bytes");
    }

    #[test]
    fn safecrlf_die_errors_on_lf_to_crlf_round_trip() {
        // autocrlf=true on a pure-LF file: checkout would add CRLF, so the
        // round-trip is irreversible and safecrlf=true dies (exit 128).
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let err = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\nb\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect_err("die must error");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn safecrlf_die_errors_on_crlf_to_lf_round_trip() {
        // autocrlf=input on a CRLF file: clean strips CRLF and checkout never
        // restores it, so safecrlf=true dies.
        let config = config_from("[core]\n\tautocrlf = input\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let err = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\r\nb\r\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect_err("die must error");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn safecrlf_reversible_round_trip_does_not_warn_or_die() {
        // A CRLF file under autocrlf=true survives the round trip (clean to LF,
        // smudge back to CRLF), so even safecrlf=true is silent.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\r\nb\r\n",
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect("reversible round trip must not die");
        assert_eq!(out.as_ref(), b"a\nb\n");
    }

    #[test]
    fn safecrlf_binary_content_is_silent() {
        // autocrlf=true with NUL-containing (binary) content: no conversion and
        // no warning/die, mirroring git's early-return in crlf_to_git.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let body: &[u8] = b"a\nb\0c\n";
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.bin",
            body,
            ConvFlags::Die,
            SafeCrlfIndexBlob::None,
        )
        .expect("binary content must not die");
        assert_eq!(out.as_ref(), body, "binary content is never converted");
    }

    #[test]
    fn safecrlf_off_is_silent_even_on_irreversible_round_trip() {
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let out = apply_clean_filter_with_attributes_cow_safecrlf(
            &config,
            &checks,
            b"f.txt",
            b"a\nb\n",
            ConvFlags::Off,
            SafeCrlfIndexBlob::None,
        )
        .expect("safecrlf=off never errors");
        // autocrlf=true does not convert on clean (only smudge), so bytes pass through.
        assert_eq!(out.as_ref(), b"a\nb\n");
    }

    #[test]
    fn autocrlf_input_normalizes_on_clean_but_not_smudge() {
        // autocrlf=input: clean normalizes to LF, smudge leaves LF as-is.
        let config = config_from("[core]\n\tautocrlf = input\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"file.txt", b"a\r\nb\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"a\nb\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"file.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, b"a\nb\n",
            "input mode must not add carriage returns"
        );
    }

    #[test]
    fn eol_crlf_attribute_drives_conversion_without_config() {
        // No core.autocrlf; the `eol=crlf` attribute alone forces conversion.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"eol".to_vec(),
            state: Some(AttributeState::Value(b"crlf".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"a.txt", b"x\r\ny\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"x\ny\n");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"a.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(smudged, b"x\r\ny\r\n");
    }

    #[test]
    fn binary_attribute_disables_eol_conversion() {
        // `-text` (binary) must leave CRLF/NUL content untouched in both
        // directions even when autocrlf=true.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"text".to_vec(),
            state: Some(AttributeState::Unset),
        }];
        let content = b"\x00\x01\r\n\x02\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"data.bin", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary file must not be CRLF-normalized");
        let smudged = apply_smudge_filter_with_attributes(&config, &checks, b"data.bin", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            smudged, content,
            "binary file must not gain carriage returns"
        );
    }

    #[test]
    fn autocrlf_auto_skips_binary_looking_content() {
        // text=auto (via autocrlf) must not convert content that contains NUL.
        let config = config_from("[core]\n\tautocrlf = true\n");
        let checks: Vec<AttributeCheck> = Vec::new();
        let content = b"a\r\n\x00b\r\n".to_vec();
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f", &content)
            .expect("test operation should succeed");
        assert_eq!(blob, content, "binary-looking content stays untouched");
    }

    #[test]
    fn autocrlf_via_add_and_checkout_round_trips() {
        // End-to-end: a CRLF worktree file is stored as an LF blob by the
        // filtered add path, and restored as CRLF by the filtered checkout.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        let config = config_from("[core]\n\tautocrlf = true\n");

        fs::write(root.join("crlf.txt"), b"alpha\r\nbeta\r\n")
            .expect("test operation should succeed");
        add_paths_to_index_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from("crlf.txt")],
            &config,
        )
        .expect("test operation should succeed");

        // The stored blob must be LF-normalized.
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"crlf.txt");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let blob = odb
            .read_object(&entry.oid)
            .expect("test operation should succeed");
        assert_eq!(blob.body, b"alpha\nbeta\n");

        // Commit and point HEAD at it, then re-checkout with smudge filtering.
        let tree = write_tree_from_index(&git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author T <t@e> 0 +0000\ncommitter T <t@e> 0 +0000\n\nm\n");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let commit = odb
            .write_object(EncodedObject::new(ObjectType::Commit, body))
            .expect("test operation should succeed");
        let refs = FileRefStore::new(&git_dir, ObjectFormat::Sha1);
        let mut tx = refs.transaction();
        tx.update(RefUpdate {
            name: "HEAD".into(),
            expected: None,
            new: RefTarget::Direct(commit),
            reflog: None,
        });
        tx.commit().expect("test operation should succeed");

        // Make the worktree match the committed (LF) blob so the tree is clean
        // for checkout; `short_status`/`worktree_entries` compare by content
        // hash and are not filter-aware. Checkout will then smudge it to CRLF.
        fs::write(root.join("crlf.txt"), b"alpha\nbeta\n").expect("test operation should succeed");
        checkout_detached_filtered(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &commit,
            b"T <t@e> 0 +0000".to_vec(),
            b"co".to_vec(),
            &config,
        )
        .expect("test operation should succeed");
        assert_eq!(
            fs::read(root.join("crlf.txt")).expect("test operation should succeed"),
            b"alpha\r\nbeta\r\n",
            "checkout must restore CRLF line endings"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn driver_filter_clean_and_smudge_transform_both_directions() {
        // filter=case: clean upper-cases (worktree -> blob), smudge lower-cases
        // (blob -> worktree).
        let config =
            config_from("[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"case".to_vec())),
        }];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"Hello World")
            .expect("test operation should succeed");
        assert_eq!(blob, b"HELLO WORLD", "clean driver must upper-case");
        let worktree =
            apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", b"HELLO WORLD")
                .expect("test operation should succeed");
        assert_eq!(worktree, b"hello world", "smudge driver must lower-case");
    }

    #[test]
    fn driver_filter_resolved_from_gitattributes_file() {
        // The filter name is read from a real `.gitattributes`, the commands from
        // config; exercises the public worktree-rooted entry points.
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join(".gitattributes"), b"*.dat filter=rot\n")
            .expect("test operation should succeed");
        let config =
            config_from("[filter \"rot\"]\n\tclean = sed s/a/b/g\n\tsmudge = sed s/b/a/g\n");
        // Clean reads attributes from the live worktree `.gitattributes`.
        let blob = apply_clean_filter(&root, &git_dir, &config, b"x.dat", b"banana")
            .expect("test operation should succeed");
        assert_eq!(blob, b"bbnbnb");
        // Smudge reads attributes from the index (the worktree file may not
        // exist yet during checkout), so stage `.gitattributes` first.
        add_paths_to_index(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &[PathBuf::from(".gitattributes")],
        )
        .expect("test operation should succeed");
        let smudged = apply_smudge_filter(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            &config,
            b"x.dat",
            &blob,
        )
        .expect("test operation should succeed");
        // sed s/b/a/g is not a perfect inverse, but verifies the smudge command
        // ran on the blob bytes.
        assert_eq!(smudged, b"aanana");
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn required_filter_failure_is_fatal() {
        // A required filter whose command fails must surface an error.
        let config = config_from("[filter \"boom\"]\n\tclean = false\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"boom".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter failure must error");
        assert!(matches!(err, GitError::Command(_)), "got {err:?}");
    }

    #[test]
    fn required_filter_missing_command_is_fatal() {
        // required=true but no clean command for this direction is also fatal.
        let config = config_from("[filter \"need\"]\n\tsmudge = cat\n\trequired = true\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"need".to_vec())),
        }];
        let err = apply_clean_filter_with_attributes(&config, &checks, b"f", b"data")
            .expect_err("required filter without a clean command must error");
        assert!(matches!(err, GitError::Command(_)), "got {err:?}");
    }

    #[test]
    fn non_required_filter_failure_passes_through() {
        // A non-required filter that fails must pass the content through
        // unchanged rather than erroring.
        let config = config_from("[filter \"opt\"]\n\tclean = false\n");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"opt".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"keepme")
            .expect("test operation should succeed");
        assert_eq!(
            out, b"keepme",
            "optional filter failure passes content through"
        );
    }

    #[test]
    fn filter_with_no_command_is_noop() {
        // filter=name with no configured commands and not required is ignored.
        let config = config_from("");
        let checks = vec![AttributeCheck {
            attribute: b"filter".to_vec(),
            state: Some(AttributeState::Value(b"ghost".to_vec())),
        }];
        let out = apply_clean_filter_with_attributes(&config, &checks, b"f", b"unchanged")
            .expect("test operation should succeed");
        assert_eq!(out, b"unchanged");
    }

    #[test]
    fn driver_and_eol_compose_on_clean_and_smudge() {
        // filter=case + autocrlf=true: clean runs the driver then CRLF->LF;
        // smudge runs LF->CRLF then the driver.
        let config = config_from(
            "[core]\n\tautocrlf = true\n[filter \"case\"]\n\tclean = tr a-z A-Z\n\tsmudge = tr A-Z a-z\n",
        );
        let checks = vec![
            AttributeCheck {
                attribute: b"filter".to_vec(),
                state: Some(AttributeState::Value(b"case".to_vec())),
            },
            AttributeCheck {
                attribute: b"text".to_vec(),
                state: Some(AttributeState::Set),
            },
        ];
        let blob = apply_clean_filter_with_attributes(&config, &checks, b"f.txt", b"ab\r\ncd\r\n")
            .expect("test operation should succeed");
        assert_eq!(blob, b"AB\nCD\n", "clean: upper-case then CRLF->LF");
        let worktree = apply_smudge_filter_with_attributes(&config, &checks, b"f.txt", &blob)
            .expect("test operation should succeed");
        assert_eq!(
            worktree, b"ab\r\ncd\r\n",
            "smudge: LF->CRLF then lower-case"
        );
    }

    #[test]
    fn attrs_helper_reads_filter_from_disk() {
        let root = temp_root();
        fs::write(root.join(".gitattributes"), b"*.txt text\n*.bin -text\n")
            .expect("test operation should succeed");
        let text = attrs(&root, b"a.txt");
        assert!(
            text.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Set))
        );
        let bin = attrs(&root, b"a.bin");
        assert!(
            bin.iter()
                .any(|c| c.attribute == b"text" && c.state == Some(AttributeState::Unset))
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    /// Builds a stat cache holding a single stage-0 entry whose size+mtime match
    /// `file`'s real metadata, with the index-file mtime placed strictly after
    /// the entry mtime so the entry reads as non-racy by default. The entry's oid
    /// is `oid` and its mode is `mode`.
    fn stat_cache_for(file: &Path, oid: ObjectId, mode: u32) -> (IndexStatCache, IndexEntry) {
        let metadata = fs::metadata(file).expect("test operation should succeed");
        let mut entry = index_entry_from_metadata(b"f.txt".to_vec(), oid, &metadata);
        entry.mode = mode;
        let index_mtime = Some((u64::from(entry.mtime_seconds) + 10, 0));
        let mut entries = HashMap::new();
        entries.insert(entry.path.as_bytes().to_vec(), entry.clone());
        (
            IndexStatCache {
                entries,
                index_mtime,
            },
            entry,
        )
    }

    #[test]
    fn reuse_tracked_entry_only_reuses_clean_non_racy_match() {
        let root = temp_root();
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let file = root.join("f.txt");
        let metadata = fs::metadata(&file).expect("test operation should succeed");
        let real_mode = file_mode(&metadata);
        let oid = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
            .object_id(ObjectFormat::Sha1)
            .expect("test operation should succeed");

        // Clean, non-racy, matching stat + mode -> reuse the cached oid.
        let (cache, _) = stat_cache_for(&file, oid, real_mode);
        let reused = cache.reuse_tracked_entry(b"f.txt", &metadata);
        assert_eq!(
            reused,
            Some(TrackedEntry {
                mode: real_mode,
                oid,
            }),
            "a clean non-racy stat+mode match must reuse the staged oid"
        );

        // No stage-0 entry for the path -> must hash.
        assert_eq!(
            cache.reuse_tracked_entry(b"other.txt", &metadata),
            None,
            "a path with no cached entry must fall through to hashing"
        );

        // Size differs from the file -> must hash.
        let (mut size_cache, mut shrunk) = stat_cache_for(&file, oid, real_mode);
        shrunk.size = shrunk.size.saturating_sub(1);
        size_cache.entries.insert(shrunk.path.to_vec(), shrunk);
        assert_eq!(
            size_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a size mismatch must fall through to hashing"
        );

        // Mode differs (e.g. a chmod that did not move mtime) -> must hash.
        let (mode_cache, _) = stat_cache_for(&file, oid, 0o100755);
        assert_eq!(
            mode_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a mode mismatch must fall through to hashing"
        );

        // Racily clean (index mtime not strictly after the entry mtime) -> hash.
        let (mut racy_cache, entry) = stat_cache_for(&file, oid, real_mode);
        racy_cache.index_mtime = Some((
            u64::from(entry.mtime_seconds),
            u64::from(entry.mtime_nanoseconds),
        ));
        assert_eq!(
            racy_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "a racily-clean entry must always be re-hashed"
        );

        // Unknown index mtime is treated as racy -> hash.
        let (mut unknown_cache, _) = stat_cache_for(
            &file,
            EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec())
                .object_id(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
            real_mode,
        );
        unknown_cache.index_mtime = None;
        assert_eq!(
            unknown_cache.reuse_tracked_entry(b"f.txt", &metadata),
            None,
            "an unknown index mtime must be treated conservatively as racy"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn index_stat_probe_cache_serves_many_paths_from_one_index_parse() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("a.txt"), b"alpha\n").expect("test operation should succeed");
        fs::write(root.join("b.txt"), b"bravo\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["a.txt", "b.txt"]);

        let cache = IndexStatProbeCache::from_repository_index(&git_dir, ObjectFormat::Sha1)
            .expect("probe cache");
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_git_path(b"a.txt"));
        assert!(cache.contains_git_path(b"b.txt"));
        let a = cache.probe_for_git_path(b"a.txt").expect("a probe");
        let b = cache.probe_for_git_path(b"b.txt").expect("b probe");
        assert_eq!(a.entry().path, b"a.txt");
        assert_eq!(b.entry().path, b"b.txt");
        assert_eq!(a.index_mtime(), cache.index_mtime());
        assert_eq!(b.index_mtime(), cache.index_mtime());
        assert!(
            cache.probe_for_git_path(b"missing.txt").is_none(),
            "missing paths should not allocate probes"
        );

        let one_shot =
            IndexStatProbe::from_repository_index(&git_dir, ObjectFormat::Sha1, b"a.txt")
                .expect("legacy one-shot probe")
                .expect("a probe");
        assert_eq!(one_shot.entry().path, b"a.txt");
        assert_eq!(one_shot.index_mtime(), cache.index_mtime());

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_detects_same_length_content_change() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"aaaa\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Overwrite with the SAME byte length but different content. Right after
        // staging the entry is racily clean (index mtime >= entry mtime), so the
        // stat shortcut must not be trusted and the change must surface as M.
        fs::write(root.join("f.txt"), b"bbbb\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert_eq!(
            status
                .iter()
                .map(ShortStatusEntry::line)
                .collect::<Vec<_>>(),
            vec![" M f.txt"],
            "a same-length content change must be reported modified"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_clean_after_byte_identical_rewrite() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        // Rewrite with byte-identical content; the mtime moves so the stat
        // shortcut declines to reuse and the fallback hash proves it clean.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "a byte-identical rewrite must be clean via the fallback hash, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_trusts_stat_cache_and_skips_rehash() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);

        // Plant a BOGUS oid in the stage-0 entry while preserving its size+mtime,
        // so a real re-hash of the (unchanged) worktree file would NOT match it.
        let index_path = repository_index_path(&git_dir);
        let mut index = read_index(&git_dir);
        let bogus = ObjectId::from_hex(ObjectFormat::Sha1, &"0".repeat(40))
            .expect("test operation should succeed");
        let real_oid = index_entry_for(&index, b"f.txt").oid;
        assert_ne!(
            real_oid, bogus,
            "fixture oid should differ from the bogus oid"
        );
        index
            .entries
            .iter_mut()
            .find(|entry| entry.path == b"f.txt")
            .expect("test operation should succeed")
            .oid = bogus.clone();
        fs::write(
            &index_path,
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // Make the index file STRICTLY newer than the entry mtime (non-racy) by
        // waiting past one-second filesystem granularity and rewriting it, so the
        // racy-clean guard does not force a re-hash.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            &index_path,
            fs::read(&index_path).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");

        // The file is unchanged on disk, so a trusted stat reuses the bogus index
        // oid for the worktree entry: worktree-oid == index-oid == bogus, so the
        // WORKTREE column is clean. Had status re-hashed the file, the real oid
        // would differ from the bogus index oid and the worktree column would be
        // 'M'. (The index-vs-HEAD column is 'M' because we corrupted the index
        // oid away from HEAD; that is expected and not what this test asserts.)
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        let entry = status
            .iter()
            .find(|entry| entry.path == b"f.txt")
            .expect("f.txt should appear (its index oid now differs from HEAD)");
        assert_eq!(
            entry.worktree, b' ',
            "non-racy stat match must trust the cached oid (no re-hash); worktree column was {}",
            entry.worktree as char
        );
        assert_eq!(
            entry.index_oid.as_ref(),
            Some(&bogus),
            "the worktree entry must have reused the planted bogus index oid, not the real hash"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_detects_same_size_content_change() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"aaaa\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();
        let probe = IndexStatProbe::from_index_entry_and_index_path(
            entry.clone(),
            repository_index_path(&git_dir),
        );

        fs::write(root.join("f.txt"), b"bbbb\n").expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_reports_deleted_for_missing_and_parent_not_directory() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("dir")).expect("test operation should succeed");
        fs::write(root.join("dir").join("f.txt"), b"hello\n")
            .expect("test operation should succeed");
        build_commit(&root, &git_dir, &["dir/f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"dir/f.txt").clone();

        fs::remove_file(root.join("dir").join("f.txt")).expect("test operation should succeed");
        let missing = worktree_entry_state_by_git_path(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            b"dir/f.txt",
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(missing, WorktreeEntryState::Deleted);

        fs::remove_dir(root.join("dir")).expect("test operation should succeed");
        fs::write(root.join("dir"), b"not a directory").expect("test operation should succeed");
        let parent_not_directory = worktree_entry_state_by_git_path(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            b"dir/f.txt",
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(parent_not_directory, WorktreeEntryState::Deleted);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_trusts_clean_non_racy_probe() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index_path = repository_index_path(&git_dir);
        let mut index = read_index(&git_dir);
        let bogus = ObjectId::from_hex(ObjectFormat::Sha1, &"1".repeat(40))
            .expect("test operation should succeed");
        index
            .entries
            .iter_mut()
            .find(|entry| entry.path == b"f.txt")
            .expect("test operation should succeed")
            .oid = bogus;
        fs::write(
            &index_path,
            index
                .write(ObjectFormat::Sha1)
                .expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(
            &index_path,
            fs::read(&index_path).expect("test operation should succeed"),
        )
        .expect("test operation should succeed");
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();
        let probe = IndexStatProbe::from_index_entry_and_index_path(
            entry.clone(),
            repository_index_path(&git_dir),
        );

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(
            state,
            WorktreeEntryState::Clean,
            "a non-racy stat match must be enough to prove this path clean"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_rehashes_racy_probe() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let mut entry = index_entry_for(&index, b"f.txt").clone();
        entry.oid = ObjectId::from_hex(ObjectFormat::Sha1, &"2".repeat(40))
            .expect("test operation should succeed");
        let probe = IndexStatProbe::from_index_entry(
            entry.clone(),
            Some((
                u64::from(entry.mtime_seconds),
                u64::from(entry.mtime_nanoseconds),
            )),
        );

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            Some(&probe),
        )
        .expect("test operation should succeed");
        assert_eq!(
            state,
            WorktreeEntryState::Modified,
            "a racily-clean stat match must fall through to hashing"
        );

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_entry_state_detects_chmod_only_change() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(root.join("f.txt"), b"hello\n").expect("test operation should succeed");
        build_commit(&root, &git_dir, &["f.txt"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"f.txt").clone();

        let file = root.join("f.txt");
        let mut permissions = fs::metadata(&file)
            .expect("test operation should succeed")
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(&file, permissions).expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("f.txt"),
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn worktree_entry_state_detects_symlink_target_change() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        symlink("one", root.join("link")).expect("test operation should succeed");
        build_commit(&root, &git_dir, &["link"]);
        let index = read_index(&git_dir);
        let entry = index_entry_for(&index, b"link").clone();

        fs::remove_file(root.join("link")).expect("test operation should succeed");
        symlink("two", root.join("link")).expect("test operation should succeed");
        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("link"),
            &entry.oid,
            entry.mode,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Modified);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn worktree_entry_state_treats_present_unpopulated_gitlink_directory_as_clean() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::create_dir_all(root.join("submodule")).expect("test operation should succeed");
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, &"3".repeat(40))
            .expect("test operation should succeed");

        let state = worktree_entry_state(
            &root,
            &git_dir,
            ObjectFormat::Sha1,
            Path::new("submodule"),
            &oid,
            sley_index::GITLINK_MODE,
            None,
        )
        .expect("test operation should succeed");
        assert_eq!(state, WorktreeEntryState::Clean);

        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn short_status_empty_on_unborn_repository() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let status = short_status(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            status.is_empty(),
            "an unborn repository with an empty worktree must be clean, got {status:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[test]
    fn untracked_paths_skips_embedded_git_internals() {
        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        let nested = root.join("not-a-submodule");
        fs::create_dir_all(nested.join(".git")).expect("test operation should succeed");
        fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(nested.join("file.txt"), b"inside\n").expect("test operation should succeed");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.iter().any(|path| path == b"not-a-submodule/"),
            "embedded repository directory should be listed, got {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(b"not-a-submodule/.git")),
            "embedded .git internals must not be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn untracked_paths_lists_symlink() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        let git_dir = root.join(".git");
        fs::create_dir_all(git_dir.join("objects")).expect("test operation should succeed");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("test operation should succeed");
        fs::write(root.join("target.txt"), b"target\n").expect("test operation should succeed");
        symlink(root.join("target.txt"), root.join("path1")).expect("create symlink");
        let paths = untracked_paths(&root, &git_dir, ObjectFormat::Sha1)
            .expect("test operation should succeed");
        assert!(
            paths.contains(&b"path1".to_vec()),
            "untracked symlink must be listed, got {paths:?}"
        );
        fs::remove_dir_all(root).expect("test operation should succeed");
    }
}
