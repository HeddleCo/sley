//! `sley-unpack-trees` — a reusable port of git's `unpack-trees.c`.
//!
//! This crate is the shared substrate behind `git read-tree`, `git checkout`,
//! `git reset`, `git merge`, and `git stash`: the "apply N trees to the index
//! (and optionally the working tree)" engine. Upstream git calls this
//! `unpack_trees()`; it walks the union of paths across the source trees plus
//! the current index, hands each path's `(index, tree1, tree2, …)` slice to a
//! per-invocation merge function ([`oneway_merge`], [`twoway_merge`],
//! [`threeway_merge`], [`bind_merge`]), and accumulates a resulting index
//! together with a set of working-tree updates ([`check_updates`]).
//!
//! ## Why this exists
//!
//! The 1-/2-/3-way merge rules, the "is this path up to date in the worktree?"
//! safety check, and the "would this clobber an untracked file?" check were
//! historically hand-rolled, privately, inside `read-tree`. Every other
//! tree-applying command (`checkout`, `reset`, `merge -m`, `stash`) needs the
//! same rules. Lifting them here makes the safe path the only path: a consumer
//! picks a merge function, supplies a [`WorktreeProbe`] for the safety checks
//! and a [`WorktreeWriter`] for the apply phase, and gets git-identical
//! semantics.
//!
//! ## Model
//!
//! Git represents everything as `struct cache_entry` (`mode`, `oid`, `stage`,
//! plus per-entry flags). We model the *merge-relevant* slice of that as
//! [`CacheEntry`]; an absent path is `None`. The driver does not own any I/O:
//! tree contents arrive as already-flattened path→entry maps (the caller reads
//! trees with whatever object store it has), worktree state is read through
//! [`WorktreeProbe`], and worktree mutation goes through [`WorktreeWriter`].
//! That keeps the merge logic pure and unit-testable, exactly mirroring how
//! upstream factors `verify_uptodate` / `verify_absent` / `check_updates` out
//! of the merge functions.
//!
//! ## Fidelity to upstream
//!
//! The merge functions are line-for-line ports of the corresponding functions
//! in `unpack-trees.c` (`oneway_merge`, `twoway_merge`, `threeway_merge`,
//! `bind_merge`). D/F conflict markers, stat writeback for updated entries, and
//! the submodule move-head probe hook are modeled here now. The remaining
//! upstream machinery that still needs follow-up — sparse-checkout / sparse
//! directories, full apply-phase D/F and ignored-file handling, and real
//! submodule worktree mutation — is flagged with precise
//! `// TODO(unpack-trees):` markers at the exact spot upstream invokes it, so a
//! later wave can wire it in without re-deriving the control flow.

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use std::collections::BTreeMap;

/// Upstream git's `MAX_UNPACK_TREES`.
pub const MAX_UNPACK_TREES: usize = 8;

/// git's `struct stat_data`: the `lstat`-derived fields the index caches so the
/// "is this path dirty / up-to-date / racy-clean" machinery (`ce_match_stat`,
/// `diff-files`, refresh) can answer without re-hashing the worktree blob.
///
/// These are exactly the keys git stores per cache entry (`fill_stat_data`):
/// ctime/mtime split into seconds + nanoseconds, the device + inode, owner
/// uid/gid, and the file size. The engine never produces these itself (it does
/// no I/O); the consumer's [`WorktreeWriter`] fills them from a real `lstat`
/// after writing a file, and the input index carries the previously-recorded
/// values forward on a kept entry. `size` is git's *munged* size
/// (`munge_st_size`): a non-zero file whose low 32 bits are zero is stored as
/// `0x8000_0000` so it isn't mistaken for the racy-smudged "size 0" sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatInfo {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    /// git's munged size (see [`StatInfo::munge_size`]).
    pub size: u32,
}

impl StatInfo {
    /// git's `munge_st_size`: a file whose size is a non-zero exact multiple of
    /// 4 GiB (so its low 32 bits are zero) is stored as `0x8000_0000` rather
    /// than `0`, so it is not mistaken for the racy-smudged "size 0" marker
    /// that forces a content re-check. Truncates to 32 bits otherwise.
    pub fn munge_size(size: u64) -> u32 {
        let truncated = size as u32;
        if truncated == 0 && size != 0 {
            0x8000_0000
        } else {
            truncated
        }
    }
}

/// The merge-relevant slice of git's `struct cache_entry`.
///
/// `mode` is the raw git file mode (`0o100644`, `0o100755`, `0o120000`,
/// `0o160000`, …). `stage` is the merge stage (0 for a resolved entry, 1/2/3
/// for base/ours/theirs conflict stages). An *absent* path is represented by
/// `None` rather than a sentinel entry, matching how the merge functions take
/// `const struct cache_entry *` arguments that may be `NULL`.
///
/// `stat` carries git's `ce_stat_data` for an entry kept from the source index
/// (so a carry-forward entry round-trips its cached `lstat` info and
/// `diff-files` keeps reporting the right clean/dirty verdict). It is `None`
/// for entries sourced from a tree (a tree has no worktree stat) and for any
/// entry the apply phase is about to (re)write — [`check_updates`] re-fills it
/// from the post-write `lstat`, exactly as git's `check_updates` sets
/// `refresh_cache` so `checkout_entry` calls `fill_stat_cache_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub mode: u32,
    pub oid: ObjectId,
    /// Merge stage 0-3.
    pub stage: u8,
    /// git's `ce_stat_data`, carried forward from the source index on a kept
    /// entry. `None` when sourced from a tree or pending a worktree write.
    pub stat: Option<StatInfo>,
    /// The logical entry is outside the sparse-checkout cone. Unpack-trees
    /// still merges it in memory, but must not materialize or remove its
    /// worktree path.
    skip_worktree: bool,
    /// git's `o->df_conflict_entry` sentinel marker. When `true`, this slot is
    /// not a real entry: it is the directory/file (D/F) conflict placeholder
    /// that `unpack_trees` synthesizes for a tree slot whose path is a
    /// *directory* (or lives under a *file* in that tree) where another source
    /// has the colliding *file* (resp. *directory*). The merge functions test
    /// it exactly as upstream tests `src[i] == o->df_conflict_entry`: it forces
    /// a file-vs-directory collision to resolve to conflict *stages* instead of
    /// a bogus stage-0 entry. The `mode`/`oid`/`stage` of a marker are inert.
    df_conflict: bool,
}

impl CacheEntry {
    /// A stage-0 (resolved) entry with no cached stat info.
    pub fn stage0(mode: u32, oid: ObjectId) -> Self {
        Self {
            mode,
            oid,
            stage: 0,
            stat: None,
            skip_worktree: false,
            df_conflict: false,
        }
    }

    /// A stage-0 (resolved) entry carrying the source index's cached stat info.
    pub fn stage0_with_stat(mode: u32, oid: ObjectId, stat: Option<StatInfo>) -> Self {
        Self {
            mode,
            oid,
            stage: 0,
            stat,
            skip_worktree: false,
            df_conflict: false,
        }
    }

    /// An entry at an explicit conflict stage (1=base, 2=ours, 3=theirs).
    pub fn staged(mode: u32, oid: ObjectId, stage: u8) -> Self {
        Self {
            mode,
            oid,
            stage,
            stat: None,
            skip_worktree: false,
            df_conflict: false,
        }
    }

    /// git's `o->df_conflict_entry`: the D/F-conflict sentinel placed into a
    /// tree slot by [`unpack_trees`]. See `CacheEntry::df_conflict`.
    pub fn df_conflict_marker() -> Self {
        Self {
            mode: 0,
            oid: ObjectId::null(ObjectFormat::Sha1),
            stage: 0,
            stat: None,
            skip_worktree: false,
            df_conflict: true,
        }
    }

    /// Whether this slot is the D/F-conflict sentinel marker (git's
    /// `ce == o->df_conflict_entry`).
    pub fn is_df_conflict(&self) -> bool {
        self.df_conflict
    }

    fn with_skip_worktree(mut self, skip_worktree: bool) -> Self {
        self.skip_worktree = skip_worktree;
        self
    }
}

/// git's `S_ISGITLINK(mode)`: the entry is a submodule (gitlink) when the file
/// type bits of its raw git mode are `0o160000`. `CacheEntry::mode` holds the
/// same raw git mode git stores (`0o100644`, `0o120000`, `0o160000`, …).
///
/// The engine does NOT re-derive the file-type-mask test — it reuses the single
/// `sley_index::is_gitlink` definition, so "what is a gitlink" has one owner
/// shared by the index, this unpack-trees engine, and every CLI consumer.
use sley_index::is_gitlink;

/// git's `same()`: two slots are equal iff both absent, or both present with
/// equal mode and oid. (Upstream additionally treats either side being
/// `CE_CONFLICTED` as "not same"; conflictedness is carried by the caller's
/// staging, so a stage-0 `CacheEntry` is never conflicted here.)
fn same(a: Option<&CacheEntry>, b: Option<&CacheEntry>) -> bool {
    match (a, b) {
        (None, None) => true,
        // A D/F-conflict marker is never "same" as anything. Upstream nulls the
        // marker out before calling `same()`, so it never reaches here in the
        // normal path; this guard keeps `same()` honest if a caller hasn't
        // unwrapped the sentinel yet (e.g. `oneway_merge`'s identical check).
        (Some(x), _) if x.df_conflict => false,
        (_, Some(y)) if y.df_conflict => false,
        (Some(x), Some(y)) => x.mode == y.mode && x.oid == y.oid,
        _ => false,
    }
}

/// `reset` mode for the worktree-clobber checks, mirroring
/// `enum unpack_trees_reset_type`. Only the two values sley exercises today are
/// modeled; `--reset` worktree resets map to [`ResetType::OverwriteUntracked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetType {
    /// Not a reset; untracked files are protected (`UNPACK_RESET_NONE`).
    #[default]
    None,
    /// `read-tree --reset`: discard untracked files in the way
    /// (`UNPACK_RESET_OVERWRITE_UNTRACKED`).
    OverwriteUntracked,
}

/// Options controlling an [`unpack_trees`] run, mirroring the fields of
/// `struct unpack_trees_options` that sley exercises.
#[derive(Debug, Clone)]
pub struct UnpackTreesOptions {
    /// `o->merge`: run a real merge (vs. a plain overlay). The merge functions
    /// always assume merge mode; this gates the up-to-date safety checks.
    pub merge: bool,
    /// `o->update`: apply the result to the working tree (`-u`).
    pub update: bool,
    /// `o->index_only`: never touch or stat the working tree.
    pub index_only: bool,
    /// `o->reset`: how aggressively to discard untracked files in the way.
    pub reset: ResetType,
    /// `o->head_idx`: which source slot is "head/ours" in a 3-way merge.
    ///
    /// The source slice is `[index, stage1, …, stageN]`; ancestors occupy slots
    /// `1..head_idx`, "head/ours" is slot `head_idx`, and "remote/theirs" is
    /// `head_idx + 1`. For a standard 3-tree `read-tree -m base ours theirs`
    /// (slots `[index, base, ours, theirs]`) git sets `head_idx = nr_trees - 1`
    /// (= 2 here). [`unpack_trees`] derives this automatically for
    /// [`MergeFn::ThreeWay`] when `head_idx` is left at its default of 1, so a
    /// plain consumer never has to set it.
    pub head_idx: usize,
    /// `o->aggressive`: resolve more trivial 3-way cases to stage 0.
    pub aggressive: bool,
    /// `o->initial_checkout`: the index started empty (affects twoway_merge's
    /// "deletion was staged" case).
    pub initial_checkout: bool,
    /// The object format (needed for the implicit empty-tree oid).
    pub format: ObjectFormat,
}

impl UnpackTreesOptions {
    /// Defaults matching git's zero-initialized options for a `read-tree -m`
    /// run: 3-way head at slot 1, no aggressive resolution, not index-only.
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            merge: true,
            update: false,
            index_only: false,
            reset: ResetType::None,
            head_idx: 1,
            aggressive: false,
            initial_checkout: false,
            format,
        }
    }
}

/// Worktree-state queries the merge functions need (`verify_uptodate` and
/// `verify_absent`). The driver never touches the filesystem directly; a
/// consumer supplies this. The defaults are the `index_only` answers (always
/// "safe"), so an index-only consumer can use the [`NullWorktree`] no-op.
pub trait WorktreeProbe {
    /// git's `verify_uptodate`: is the working-tree copy of `path` consistent
    /// with the index entry `ce` the merge is about to replace or remove?
    ///
    /// Returns `Ok(())` when the path is up to date (safe to proceed) and an
    /// error (git's `ERROR_NOT_UPTODATE_FILE`) when a local modification would
    /// be lost. `ce` is the *current index* entry for the path.
    fn verify_uptodate(&self, path: &[u8], ce: &CacheEntry) -> Result<()>;

    /// git's `verify_absent`: would materializing a *newly added* `path`
    /// clobber an untracked working-tree file? Returns `Ok(())` when the path
    /// is clear and an error (`ERROR_WOULD_LOSE_UNTRACKED_OVERWRITTEN`) when an
    /// untracked file is in the way.
    ///
    /// `merge` is the entry about to be written. `reset` carries the option so
    /// `--reset` can authorize overwriting untracked files.
    fn verify_absent_overwrite(
        &self,
        path: &[u8],
        merge: &CacheEntry,
        reset: ResetType,
    ) -> Result<()>;

    /// git's `verify_absent(… ERROR_WOULD_LOSE_UNTRACKED_REMOVED …)`: would
    /// removing `path` discard an untracked working-tree file at that name?
    fn verify_absent_remove(&self, path: &[u8], reset: ResetType) -> Result<()>;

    /// git's `check_submodule_move_head` (`unpack-trees.c`): would moving this
    /// gitlink's HEAD from `old_oid` (`None` when the submodule is newly
    /// appearing in the target tree) to `new_oid` lose uncommitted submodule
    /// work? The engine only calls this for entries whose mode is a gitlink
    /// (`S_ISGITLINK`); the probe owns the rest of git's guard condition
    /// (`submodule_from_ce(ce) && file_exists(ce->name)`), returning `Ok(())`
    /// when the path is not a real, populated submodule.
    ///
    /// Returns `Ok(())` when the move is safe and an error
    /// (git's `ERROR_WOULD_LOSE_SUBMODULE`) when it would discard work. The
    /// default is `Ok(())` so index-only / non-submodule-aware consumers (and
    /// [`NullWorktree`]) keep compiling and behaving exactly as before.
    fn check_submodule_move_head(
        &self,
        path: &[u8],
        old_oid: Option<&ObjectId>,
        new_oid: &ObjectId,
        reset: ResetType,
    ) -> Result<()> {
        let _ = (path, old_oid, new_oid, reset);
        Ok(())
    }
}

/// A [`WorktreeProbe`] that always answers "safe" — the correct behaviour for
/// an `index_only` / `--no-update` run, where no working-tree file is at risk.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullWorktree;

impl WorktreeProbe for NullWorktree {
    fn verify_uptodate(&self, _path: &[u8], _ce: &CacheEntry) -> Result<()> {
        Ok(())
    }
    fn verify_absent_overwrite(
        &self,
        _path: &[u8],
        _merge: &CacheEntry,
        _reset: ResetType,
    ) -> Result<()> {
        Ok(())
    }
    fn verify_absent_remove(&self, _path: &[u8], _reset: ResetType) -> Result<()> {
        Ok(())
    }
}

/// What the apply phase ([`check_updates`]) should do to a path's working-tree
/// copy, derived from the merge result's flags (git's `CE_UPDATE` /
/// `CE_WT_REMOVE`).
#[derive(Debug, Clone)]
pub enum WorktreeAction {
    /// Write `(mode, oid)`'s blob to the working tree (git's `CE_UPDATE`).
    Write { mode: u32, oid: ObjectId },
    /// Remove the path from the working tree (git's `CE_WT_REMOVE`).
    Remove,
}

/// The worktree-mutation side of the apply phase. A consumer supplies how to
/// write a blob and how to remove a path; [`check_updates`] sequences the calls
/// the way git's `check_updates` does (removals first, then writes).
pub trait WorktreeWriter {
    /// Materialize `oid` at `path` with `mode`, returning the post-write
    /// `lstat` data git records back into the index entry.
    ///
    /// This is git's `checkout_entry` with `state.force = 1` and
    /// `state.refresh_cache = 1`: the implementation must
    ///
    /// * remove anything already at `path` that is in the way — a regular file,
    ///   a symlink, **or a whole directory subtree** (the D/F case where a path
    ///   that was a directory is being replaced by a file);
    /// * create any leading directories, removing a file in the way of a needed
    ///   directory component (git's `create_directories`);
    /// * write the content as the right *type* for `mode` — a regular file for
    ///   `0o100644`/`0o100755`, a **symlink** whose target is the blob bytes for
    ///   `0o120000`;
    /// * `lstat` the result and return its [`StatInfo`] so [`check_updates`] can
    ///   store it back into the entry (git's `fill_stat_cache_info`).
    ///
    /// Returning `Ok(None)` means "no stat available" (e.g. the platform could
    /// not `lstat` the written path); the entry then keeps an all-zero stat,
    /// which git treats as "needs a refresh / racily clean".
    fn write_blob(&mut self, path: &[u8], mode: u32, oid: &ObjectId) -> Result<Option<StatInfo>>;
    /// Remove `path` from the working tree (idempotent on an absent target).
    /// This is git's `unlink_entry`.
    fn remove_path(&mut self, path: &[u8]) -> Result<()>;
}

/// One resolved row of the result index, plus the working-tree action the merge
/// decided on (if any). This is git's post-merge `o->internal.result` cache
/// entry carrying its `CE_UPDATE` / `CE_WT_REMOVE` flags.
#[derive(Debug, Clone)]
pub struct ResultEntry {
    pub path: Vec<u8>,
    pub entry: CacheEntry,
    /// `true` when git set `CE_UPDATE` on this entry (the worktree blob must be
    /// (re)written during [`check_updates`]).
    pub wt_update: bool,
}

/// A removal recorded by the merge (git's `add_entry(… CE_REMOVE …)`): the path
/// is dropped from the index, and — outside `index_only` — from the worktree.
#[derive(Debug, Clone)]
struct Removal {
    path: Vec<u8>,
}

/// Mutable accumulator threaded through the merge functions — git's
/// `o->internal.result` plus the `nontrivial_merge` flag.
pub struct UnpackTreesState {
    result: Vec<ResultEntry>,
    removals: Vec<Removal>,
    /// git's `o->internal.nontrivial_merge`: set when threeway_merge falls
    /// through to emitting conflict stages.
    nontrivial_merge: bool,
}

impl UnpackTreesState {
    fn new() -> Self {
        Self {
            result: Vec::new(),
            removals: Vec::new(),
            nontrivial_merge: false,
        }
    }

    /// git's `add_entry(o, ce, CE_UPDATE?, …)` for a kept/merged entry.
    fn add_entry(&mut self, path: &[u8], entry: CacheEntry, wt_update: bool) {
        self.result.push(ResultEntry {
            path: path.to_vec(),
            entry,
            wt_update,
        });
    }

    /// git's `add_entry(o, ce, CE_REMOVE, 0)`.
    fn add_removal(&mut self, path: &[u8]) {
        self.removals.push(Removal {
            path: path.to_vec(),
        });
    }

    /// Whether the 3-way merge produced conflict (stage > 0) entries.
    pub fn nontrivial_merge(&self) -> bool {
        self.nontrivial_merge
    }
}

/// The outcome of an [`unpack_trees`] run: the resulting index rows (sorted by
/// path then stage, the order git's `ls-files --stage` emits) and the
/// working-tree removals the merge decided on. Pass it to [`check_updates`] to
/// apply the worktree side.
#[derive(Debug, Clone)]
pub struct UnpackTreesResult {
    /// Resulting index entries, sorted `(path, stage)`.
    pub entries: Vec<ResultEntry>,
    /// Paths the merge removed (index + worktree).
    pub removed_paths: Vec<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// The merge primitives (per-path), ported from unpack-trees.c.
//
// Each takes the per-path source slice `src` — `src[0]` is the current index
// entry, `src[1..]` are the corresponding tree entries — plus the mutable
// state and options. They return `Ok(())` on success and an error (carrying
// git's exit semantics) when a worktree safety check rejects the path.
// ---------------------------------------------------------------------------

/// git's `oneway_merge`: take the tree wholesale into the index (a
/// fast-forward / `--reset` style read). `src = [index, tree]`.
///
/// Rule: take the stat info from the index entry, the data from the tree.
pub fn oneway_merge<P: WorktreeProbe + ?Sized>(
    src: &[Option<CacheEntry>],
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    let old = src[0].as_ref();
    // git: `if (!a || a == o->df_conflict_entry) return deleted_entry(old,…)` —
    // the tree has nothing real here (absent, or a directory shadowing this
    // file via the D/F marker), so the index entry is dropped.
    let a = src[1].as_ref().filter(|e| !e.is_df_conflict());

    let Some(a) = a else {
        return deleted_entry(old, path, state, opts, probe);
    };

    if let Some(old) = old
        && same(Some(old), Some(a))
    {
        // Identical: keep the index entry (preserving its stat info). Under
        // `reset && update` git re-checks the worktree and may flag CE_UPDATE;
        // sley's reset apply re-materializes unconditionally, so we keep the
        // entry without an update flag and let the consumer's reset path drive
        // the worktree (read_tree's `--reset -u` rewrites everything).
        state.add_entry(path, old.clone(), false);
        return Ok(());
    }
    merged_entry(a, old, path, state, opts, probe)
}

/// git's `twoway_merge`: switch the index from `oldtree` to `newtree`,
/// carrying forward local additions. `src = [index, oldtree, newtree]`.
pub fn twoway_merge<P: WorktreeProbe + ?Sized>(
    src: &[Option<CacheEntry>],
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    let current = src[0].as_ref();
    // git: `if (oldtree == o->df_conflict_entry) oldtree = NULL;` — a D/F marker
    // means the tree has a *directory* where the index/other-tree has a file, so
    // there is no real file to merge from that side; treat it as absent.
    let oldtree = src[1].as_ref().filter(|e| !e.is_df_conflict());
    let newtree = src[2].as_ref().filter(|e| !e.is_df_conflict());

    if let Some(current) = current {
        // (sley does not carry CE_CONFLICTED into a stage-0 `current`, so the
        // upstream `current->ce_flags & CE_CONFLICTED` branch is unreachable
        // here; the conflict-resolution arm lives in the consumer that builds
        // higher-stage `src[0]` slots, a TODO for the merge/stash pilots.)

        if (oldtree.is_none() && newtree.is_none())                       // 4, 5
            || (oldtree.is_none() && same(Some(current), newtree))        // 6, 7
            || (oldtree.is_some()
                && newtree.is_some()
                && same(oldtree, newtree))                                // 14, 15
            || (oldtree.is_some()
                && newtree.is_some()
                && !same(oldtree, newtree)
                && same(Some(current), newtree))
        // 18, 19
        {
            return keep_entry(current, path, state);
        } else if oldtree.is_some() && newtree.is_none() && same(Some(current), oldtree) {
            // 10 or 11
            return deleted_entry(Some(current), path, state, opts, probe);
        } else if let (Some(_), Some(newtree)) = (oldtree, newtree)
            && same(Some(current), oldtree)
            && !same(Some(current), Some(newtree))
        {
            // 20 or 21
            return merged_entry(newtree, Some(current), path, state, opts, probe);
        } else {
            // TODO(unpack-trees): the sparse-directory D/F-conflict and
            // sparse-by-OID merge arms (S_ISSPARSEDIR) live here in upstream;
            // sley does not model the sparse index yet, so all of them collapse
            // to the reject below.
            return reject_merge(current, path);
        }
    } else if let Some(newtree) = newtree {
        if let Some(oldtree) = oldtree
            && !opts.initial_checkout
        {
            // deletion of the path was staged
            if same(Some(oldtree), Some(newtree)) {
                return Ok(());
            }
            return reject_merge(oldtree, path);
        }
        return merged_entry(newtree, current, path, state, opts, probe);
    }
    deleted_entry(current, path, state, opts, probe)
}

/// git's `bind_merge`: keep the index entry, fold in a single tree, refusing an
/// overlap. `src = [index, tree]`. Used by `read-tree --prefix`.
pub fn bind_merge<P: WorktreeProbe + ?Sized>(
    src: &[Option<CacheEntry>],
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    let old = src[0].as_ref();
    let a = src[1].as_ref();

    if a.is_some() && old.is_some() {
        let display = String::from_utf8_lossy(path);
        eprintln!("error: Entry '{display}' overlaps with '{display}'.  Cannot bind.");
        return Err(GitError::Exit(128));
    }
    match (a, old) {
        (Some(a), _) => merged_entry(a, None, path, state, opts, probe),
        // `a` absent: carry the existing index entry through unchanged.
        (None, Some(old)) => keep_entry(old, path, state),
        // Neither present — the driver never iterates such a path, but treat it
        // as a no-op rather than panicking.
        (None, None) => Ok(()),
    }
}

/// git's `threeway_merge`: the trivial 3-way merge that resolves what it can to
/// stage 0 and records base/ours/theirs (stages 1/2/3) otherwise.
/// `stages = [index, ancestor..., ours, theirs]` with `head_idx` pointing at
/// the ours slot.
pub fn threeway_merge<P: WorktreeProbe + ?Sized>(
    stages: &[Option<CacheEntry>],
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    let index = stages[0].as_ref();
    let mut head = stages[opts.head_idx].as_ref();
    let mut remote = stages[opts.head_idx + 1].as_ref();

    // git's `if (head == o->df_conflict_entry) { df_conflict_head = 1; head =
    // NULL; }` (and the same for remote): a D/F marker on the head/remote side
    // means that side has a *directory* (or sits under a *file*) where the other
    // side has the colliding file — there is no real entry to merge, but the
    // marker is *not* the same as a plain deletion, so the `df_conflict_*` flags
    // suppress the trivial-resolution arms (#13/#14) that would otherwise pick a
    // bogus stage-0 result.
    let mut df_conflict_head = false;
    let mut df_conflict_remote = false;
    if head.is_some_and(CacheEntry::is_df_conflict) {
        df_conflict_head = true;
        head = None;
    }
    if remote.is_some_and(CacheEntry::is_df_conflict) {
        df_conflict_remote = true;
        remote = None;
    }

    let mut head_match = 0usize;
    let mut remote_match = 0usize;

    let mut any_anc_missing = false;
    let mut no_anc_exists = true;
    for anc in &stages[1..opts.head_idx] {
        // git: `if (!stages[i] || stages[i] == o->df_conflict_entry)` — a marker
        // ancestor counts as missing for the any/no-anc bookkeeping.
        if anc.as_ref().is_none_or(CacheEntry::is_df_conflict) {
            any_anc_missing = true;
        } else {
            no_anc_exists = false;
        }
    }

    // #16 detection: if remote != head, find which ancestors match each side.
    // The 1-based ancestor slot index is the match value, so it is kept.
    if !same(remote, head) {
        for (offset, anc) in stages[1..opts.head_idx].iter().enumerate() {
            let i = offset + 1;
            if same(anc.as_ref(), head) {
                head_match = i;
            }
            if same(anc.as_ref(), remote) {
                remote_match = i;
            }
        }
    }

    // #14, #14ALT, #2ALT — the index may match the result rather than head.
    // git: `if (remote && !df_conflict_head && head_match && !remote_match)` —
    // a D/F head suppresses this trivial "take remote" resolution.
    if let Some(remote) = remote
        && !df_conflict_head
        && head_match != 0
        && remote_match == 0
    {
        if let Some(index) = index
            && !same(Some(index), Some(remote))
            && !same(Some(index), head)
        {
            // TODO(unpack-trees): S_ISSPARSEDIR(index) → merged_sparse_dir.
            return reject_merge(index, path);
        }
        return merged_entry(remote, index, path, state, opts, probe);
    }

    // If there is an index entry, it must match head.
    if let Some(index) = index
        && !same(Some(index), head)
    {
        // TODO(unpack-trees): S_ISSPARSEDIR(index) → merged_sparse_dir.
        return reject_merge(index, path);
    }

    if let Some(head) = head {
        // #5ALT, #15
        if same(Some(head), remote) {
            return merged_entry(head, index, path, state, opts, probe);
        }
        // #13, #3ALT — git: `if (!df_conflict_remote && remote_match &&
        // !head_match)`. A D/F remote suppresses this trivial "take head"
        // resolution.
        if !df_conflict_remote && remote_match != 0 && head_match == 0 {
            return merged_entry(head, index, path, state, opts, probe);
        }
    }

    // #1
    if head.is_none() && remote.is_none() && any_anc_missing {
        return Ok(());
    }

    // Aggressive rule: resolve trivial cases git-merge-one-file used to.
    if opts.aggressive {
        let head_deleted = head.is_none();
        let remote_deleted = remote.is_none();
        let mut ce: Option<&CacheEntry> = None;
        if index.is_some() {
            ce = index;
        } else if head.is_some() {
            ce = head;
        } else if remote.is_some() {
            ce = remote;
        } else {
            // git: `if (stages[i] && stages[i] != o->df_conflict_entry)`.
            for stage in stages.iter().take(opts.head_idx).skip(1) {
                if let Some(s) = stage.as_ref().filter(|e| !e.is_df_conflict()) {
                    ce = Some(s);
                    break;
                }
            }
        }

        // Deleted in both, or deleted in one and unchanged in the other.
        if (head_deleted && remote_deleted)
            || (head_deleted && remote.is_some() && remote_match != 0)
            || (remote_deleted && head.is_some() && head_match != 0)
        {
            if let Some(index) = index {
                return deleted_entry(Some(index), path, state, opts, probe);
            }
            // git checks `ce && !head_deleted` here; `ce` only supplies the
            // path name (which we already carry), so the presence test is all
            // that remains.
            if ce.is_some() && !head_deleted {
                probe.verify_absent_remove(path, opts.reset)?;
            }
            return Ok(());
        }
        // Added in both, identically.
        if let (true, Some(head), Some(_)) = (no_anc_exists, head, remote)
            && same(Some(head), remote)
        {
            return merged_entry(head, index, path, state, opts, probe);
        }
    }

    // "No merge" cases (t1000-read-tree-m-3way): ensure the index is up to date
    // so conflict-resolution files don't clobber local work.
    if let Some(index) = index {
        // TODO(unpack-trees): S_ISSPARSEDIR(index) → merged_sparse_dir.
        if opts.merge && !opts.index_only {
            probe.verify_uptodate(path, index)?;
        }
    }

    state.nontrivial_merge = true;

    // #2, #3, #4, #6, #7, #9, #10, #11 — emit surviving stages.
    // git: `if (stages[i] && stages[i] != o->df_conflict_entry)` — never keep a
    // D/F marker as a stage entry.
    if head_match == 0 || remote_match == 0 {
        for stage in stages.iter().take(opts.head_idx).skip(1) {
            if let Some(s) = stage.as_ref().filter(|e| !e.is_df_conflict()) {
                keep_entry(s, path, state)?;
                break;
            }
        }
    }
    if let Some(head) = head {
        keep_entry(head, path, state)?;
    }
    if let Some(remote) = remote {
        keep_entry(remote, path, state)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers, ported from unpack-trees.c.
// ---------------------------------------------------------------------------

/// git's `merged_entry`: write `ce` into the result, running the appropriate
/// worktree safety check (verify_absent for a brand-new path, verify_uptodate
/// when replacing a differing index entry). Sets `CE_UPDATE` unless the entry
/// is byte-identical to the old index entry.
fn merged_entry<P: WorktreeProbe + ?Sized>(
    ce: &CacheEntry,
    old: Option<&CacheEntry>,
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    let mut update = true; // CE_UPDATE
    // git's `do_add_entry(o, merge, update, CE_STAGEMASK)` clears the stage:
    // a merged entry always lands at stage 0, regardless of which slot it came
    // from. A freshly-merged entry starts with no cached stat (it will be
    // filled by the apply phase's post-write `lstat`); the `same(old)` branch
    // below copies the old entry's stat over, mirroring git's
    // `copy_cache_entry(merge, old)`.
    let mut merge = CacheEntry::stage0(ce.mode, ce.oid)
        .with_skip_worktree(ce.skip_worktree || old.is_some_and(|entry| entry.skip_worktree));

    match old {
        None => {
            // New index entry: verify it won't clobber an untracked file.
            if opts.merge && opts.update && !opts.index_only {
                probe.verify_absent_overwrite(path, &merge, opts.reset)?;
            }
            // git's `check_submodule_move_head(o, ce, NULL, oid_to_hex(&ce->oid))`:
            // a submodule coming into existence (old == NULL). The cheap gitlink
            // mode gate is ours; the `submodule_from_ce && file_exists` half is
            // the probe's (it answers Ok when the path is not a real, populated
            // submodule). The default probe impl is a no-op, so index-only
            // consumers are unaffected.
            if is_gitlink(merge.mode) {
                probe.check_submodule_move_head(path, None, &merge.oid, opts.reset)?;
            }
        }
        Some(old) => {
            // Re-use the old entry directly when identical (keeps stat info and
            // drops CE_UPDATE so we don't overwrite local changes). git's
            // `copy_cache_entry(merge, old)`: carry the old cached `lstat` over
            // so a no-op merge leaves `diff-files` reporting the path clean.
            if same(Some(old), Some(&merge)) {
                update = false;
                merge.stat = old.stat;
            } else if opts.merge && !opts.index_only {
                probe.verify_uptodate(path, old)?;
            }
            // git's `check_submodule_move_head(o, ce, oid_to_hex(&old->oid),
            // oid_to_hex(&ce->oid))`: the gitlink's HEAD is moving from the old
            // recorded commit to the new one. Same gitlink-mode gate; the probe
            // owns `submodule_from_ce && file_exists`. Gate on the NEW entry's
            // mode (the one being written) to mirror git's `S_ISGITLINK(ce->mode)`.
            if is_gitlink(merge.mode) {
                probe.check_submodule_move_head(path, Some(&old.oid), &merge.oid, opts.reset)?;
            }
        }
    }

    let update_worktree = update && opts.update && !opts.index_only && !merge.skip_worktree;
    state.add_entry(path, merge, update_worktree);
    Ok(())
}

/// git's `deleted_entry`: drop the path. Runs verify_absent (untracked-removed)
/// when nothing tracked it, verify_uptodate otherwise.
///
/// Upstream's first argument `ce` is the entry being removed; its only role is
/// to name the path (`ce->name`), which we already carry as `path`, so it is
/// elided here.
fn deleted_entry<P: WorktreeProbe + ?Sized>(
    old: Option<&CacheEntry>,
    path: &[u8],
    state: &mut UnpackTreesState,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<()> {
    match old {
        None => {
            if opts.merge && opts.update && !opts.index_only {
                probe.verify_absent_remove(path, opts.reset)?;
            }
            return Ok(());
        }
        Some(old) => {
            if opts.merge && !opts.index_only {
                probe.verify_uptodate(path, old)?;
            }
        }
    }
    if !old.is_some_and(|entry| entry.skip_worktree) {
        state.add_removal(path);
    }
    Ok(())
}

/// git's `keep_entry`: carry an entry through unchanged (no worktree update).
fn keep_entry(ce: &CacheEntry, path: &[u8], state: &mut UnpackTreesState) -> Result<()> {
    state.add_entry(path, ce.clone(), false);
    Ok(())
}

/// git's `reject_merge`: abort the whole operation reporting the offending
/// path. Maps to git's `ERROR_WOULD_OVERWRITE` exit.
fn reject_merge(ce: &CacheEntry, path: &[u8]) -> Result<()> {
    let _ = ce;
    let display = String::from_utf8_lossy(path);
    eprintln!("error: Entry '{display}' would be overwritten by merge. Cannot merge.");
    Err(GitError::Exit(128))
}

// ---------------------------------------------------------------------------
// The driver.
// ---------------------------------------------------------------------------

/// Which merge primitive an [`unpack_trees`] run dispatches to per path,
/// mirroring `o->fn`. The number of trees the caller supplies must match
/// (1 for oneway/bind, 2 for twoway, 3 or more for threeway).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeFn {
    /// `oneway_merge` — 1 tree.
    OneWay,
    /// `twoway_merge` — 2 trees.
    TwoWay,
    /// `threeway_merge` — 3 or more trees.
    ThreeWay,
    /// `bind_merge` — 1 tree, into the current index under a prefix.
    Bind,
}

impl MergeFn {
    fn accepts_tree_count(self, count: usize) -> bool {
        match self {
            MergeFn::OneWay | MergeFn::Bind => count == 1,
            MergeFn::TwoWay => count == 2,
            MergeFn::ThreeWay => (3..=MAX_UNPACK_TREES).contains(&count),
        }
    }

    fn tree_count_description(self) -> &'static str {
        match self {
            MergeFn::OneWay | MergeFn::Bind => "1",
            MergeFn::TwoWay => "2",
            MergeFn::ThreeWay => "3..=8",
        }
    }
}

/// A flattened tree: path → (mode, oid). This is exactly the shape
/// `sley_diff_merge::flatten_tree` returns, so a consumer feeds those straight
/// in. A tree leaf has no worktree stat, so the engine seeds tree-sourced
/// entries with no [`StatInfo`].
pub type FlatTree = BTreeMap<Vec<u8>, (u32, ObjectId)>;

/// One stage-0 entry of the current index as the engine consumes it:
/// `(mode, oid, cached lstat info)`. The stat is what `git update-index` /
/// the previous checkout recorded; the engine carries it forward on a kept
/// entry so `diff-files` keeps reporting the right clean/dirty verdict, and the
/// apply phase re-fills it on a (re)written entry. `None` for a fresh
/// `intent-to-add`-style entry whose stat git stores as all-zero.
pub type IndexInputEntry = (u32, ObjectId, Option<StatInfo>);

/// The current index as a flat stage-0 map plus its sparse worktree policy.
///
/// Collapsed sparse directories are expanded only in this logical view. Their
/// prefixes remain recorded here so entries changed or newly added below them
/// participate in the merge without being materialized in the worktree.
#[derive(Debug, Clone, Default)]
pub struct FlatIndex {
    entries: BTreeMap<Vec<u8>, IndexInputEntry>,
    skip_worktree_paths: std::collections::BTreeSet<Vec<u8>>,
    sparse_directory_prefixes: Vec<Vec<u8>>,
}

impl FlatIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, path: Vec<u8>, entry: IndexInputEntry) -> Option<IndexInputEntry> {
        self.entries.insert(path, entry)
    }

    /// Record an ordinary full-index entry carrying CE_SKIP_WORKTREE.
    pub fn mark_skip_worktree(&mut self, path: Vec<u8>) {
        self.skip_worktree_paths.insert(path);
    }

    /// Record the trailing-slash prefix of a collapsed sparse directory.
    pub fn mark_sparse_directory(&mut self, prefix: Vec<u8>) {
        self.sparse_directory_prefixes.push(prefix);
    }

    fn is_skip_worktree(&self, path: &[u8]) -> bool {
        self.skip_worktree_paths.contains(path)
            || self
                .sparse_directory_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
    }
}

impl std::ops::Deref for FlatIndex {
    type Target = BTreeMap<Vec<u8>, IndexInputEntry>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl FromIterator<(Vec<u8>, IndexInputEntry)> for FlatIndex {
    fn from_iter<T: IntoIterator<Item = (Vec<u8>, IndexInputEntry)>>(iter: T) -> Self {
        Self {
            entries: iter.into_iter().collect(),
            ..Self::default()
        }
    }
}

/// Whether a tree slot for `path` should be git's `o->df_conflict_entry` marker.
///
/// `tree` is a flat path→entry map for one source (the index is never a marker,
/// so it isn't passed here). `path` is a leaf in *some* source (it is a member
/// of the traversal's path union) but is *not* a leaf in this `tree`. The slot
/// is a D/F-conflict marker iff this `tree` puts a *directory* or a *file* in
/// the way of that leaf:
///
/// * **directory side** (git's `dirmask`): `tree` has a path under `path/` — so
///   `path` names a directory here, colliding with the file another source
///   carries at `path`.
/// * **file-ancestor side** (git's propagated `df_conflicts`): a strict
///   *ancestor directory* of `path` is a *file* leaf in `tree` — so `path`
///   lives under a file here, colliding with the directory another source has.
///   This walks every ancestor, so the recursive case (`a/b` file vs
///   `a/b/c/d`, vs `a/b/c/d/e/…`) is covered at any depth.
fn df_conflict_slot(tree: &FlatTree, path: &[u8]) -> bool {
    // Directory side: is there any key strictly under `path/`?
    let mut dir_prefix = path.to_vec();
    dir_prefix.push(b'/');
    if tree
        .range(dir_prefix.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&dir_prefix))
    {
        return true;
    }

    // File-ancestor side: is any strict ancestor directory of `path` a file
    // leaf in this tree? Walk the '/'-separated prefixes shortest→longest;
    // any one of them being a leaf poisons every descendant slot.
    let mut idx = 0;
    while let Some(off) = path[idx..].iter().position(|&b| b == b'/') {
        let cut = idx + off;
        if tree.contains_key(&path[..cut]) {
            return true;
        }
        idx = cut + 1;
    }
    false
}

/// Checkout-specific controls for a two-tree index/worktree transition.
///
/// The engine derives `initial_checkout` from the supplied index and fixes the
/// remaining unpack-trees mode to Git's checkout contract: merge and worktree
/// updates enabled, index-only disabled, and the two-way merge primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckoutTransitionOptions {
    /// Repository object format, used for the empty old tree of an unborn HEAD.
    pub format: ObjectFormat,
    /// Permit tracked and untracked paths in the way to be overwritten (`-f`).
    pub overwrite_untracked: bool,
}

impl CheckoutTransitionOptions {
    /// Construct safe, non-forcing checkout transition options.
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            format,
            overwrite_untracked: false,
        }
    }
}

/// A validated checkout transition plan before its worktree writes are applied.
///
/// Consumers may inspect the pure merge result for additional repository-level
/// safety checks, then call [`CheckoutTransitionPlan::apply`] exactly once.
#[derive(Debug)]
pub struct CheckoutTransitionPlan {
    result: UnpackTreesResult,
    unpack_options: UnpackTreesOptions,
}

impl CheckoutTransitionPlan {
    /// Pure resulting index entries and planned worktree removals.
    pub fn result(&self) -> &UnpackTreesResult {
        &self.result
    }

    /// Apply planned removals and writes, returning the stat-refreshed result.
    pub fn apply<W: WorktreeWriter>(mut self, writer: &mut W) -> Result<UnpackTreesResult> {
        check_updates(&mut self.result, &self.unpack_options, writer)?;
        Ok(self.result)
    }
}

/// Plan Git's checkout/switch two-tree transition.
///
/// `old_tree` is absent for an unborn HEAD. The operation owns checkout's
/// unpack-trees semantics rather than requiring each porcelain caller to
/// reconstruct `MergeFn::TwoWay`, reset mode, initial-checkout detection, and
/// update/index-only flags independently.
pub fn plan_checkout_transition<P: WorktreeProbe + ?Sized>(
    index: &FlatIndex,
    old_tree: Option<FlatTree>,
    new_tree: FlatTree,
    options: CheckoutTransitionOptions,
    probe: &P,
) -> Result<CheckoutTransitionPlan> {
    let mut unpack_options = UnpackTreesOptions::new(options.format);
    unpack_options.merge = true;
    unpack_options.update = true;
    unpack_options.index_only = false;
    unpack_options.initial_checkout = index.is_empty();
    if options.overwrite_untracked {
        unpack_options.reset = ResetType::OverwriteUntracked;
    }
    let trees = [old_tree.unwrap_or_default(), new_tree];
    let result = unpack_trees(index, &trees, MergeFn::TwoWay, &unpack_options, probe)?;
    Ok(CheckoutTransitionPlan {
        result,
        unpack_options,
    })
}

/// Run git's `unpack_trees`: walk the union of paths across `index` and the
/// supplied `trees`, dispatch each path's source slice to `merge_fn`, and
/// accumulate the resulting index plus worktree removals.
///
/// `trees.len()` must equal the merge function's arity. The result entries are
/// returned sorted by `(path, stage)` — the order `git ls-files --stage`
/// emits — so a consumer can serialize them directly into the index.
///
/// The worktree safety checks (`verify_uptodate` / `verify_absent`) run through
/// `probe`; pass [`NullWorktree`] for an index-only / `--no-update` run.
pub fn unpack_trees<P: WorktreeProbe + ?Sized>(
    index: &FlatIndex,
    trees: &[FlatTree],
    merge_fn: MergeFn,
    opts: &UnpackTreesOptions,
    probe: &P,
) -> Result<UnpackTreesResult> {
    if !merge_fn.accepts_tree_count(trees.len()) {
        return Err(GitError::InvalidFormat(format!(
            "unpack_trees: {:?} needs {} tree(s), got {}",
            merge_fn,
            merge_fn.tree_count_description(),
            trees.len()
        )));
    }

    // git's read-tree sets head_idx = nr_trees - 1 for a >=3-tree merge (the
    // ancestors occupy slots 1..head_idx). Derive it here when the caller left
    // the default so a plain 3-way consumer doesn't have to know the formula.
    let mut effective = opts.clone();
    if merge_fn == MergeFn::ThreeWay && opts.head_idx == 1 && trees.len() >= 3 {
        effective.head_idx = trees.len() - 1;
    }
    if merge_fn == MergeFn::ThreeWay && effective.head_idx + 1 > trees.len() {
        return Err(GitError::InvalidFormat(format!(
            "unpack_trees: invalid head_idx {} for {} tree(s)",
            effective.head_idx,
            trees.len()
        )));
    }
    let opts = &effective;

    // The union of every path across the index and all trees, in sorted order
    // (git walks the merged tree traversal in name order; the BTree union
    // reproduces that ordering for the flat model).
    let mut paths: std::collections::BTreeSet<&Vec<u8>> = std::collections::BTreeSet::new();
    paths.extend(index.keys());
    for tree in trees {
        paths.extend(tree.keys());
    }

    let mut state = UnpackTreesState::new();
    // src[0] = index, src[1..] = each tree, reused per path.
    let mut src: Vec<Option<CacheEntry>> = vec![None; trees.len() + 1];

    for path in paths {
        // git loads each source tree's entries at a stage equal to its slot
        // (the index is slot 0/stage 0, tree 1 is stage 1, …). `keep_entry`
        // then carries those stages into the conflict result, while
        // `merged_entry` clears the stage to 0 (git's `CE_STAGEMASK`).
        //
        // git never places a D/F marker in the index slot (`src[0]`): a
        // directory in the index is handled by descending into it, and
        // `find_cache_entry` returns nothing for a directory prefix, so the
        // index slot is simply absent when this path is not a real index leaf.
        src[0] = index.get(path).map(|(mode, oid, stat)| {
            CacheEntry::stage0_with_stat(*mode, *oid, *stat)
                .with_skip_worktree(index.is_skip_worktree(path))
        });
        for (i, tree) in trees.iter().enumerate() {
            let slot = i + 1;
            let stage = if !opts.merge {
                0
            } else if slot < opts.head_idx {
                1
            } else if slot > opts.head_idx {
                3
            } else {
                2
            };
            src[i + 1] = match tree.get(path) {
                // A real leaf in this tree: take it at its slot's stage.
                Some((mode, oid)) => Some(
                    CacheEntry::staged(*mode, *oid, stage)
                        .with_skip_worktree(index.is_skip_worktree(path)),
                ),
                // Not a leaf here. If this path is a *directory* in this tree
                // (some `path/…` exists) — git's `dirmask` bit — or an *ancestor
                // directory* of this path is a *file* leaf in this tree — git's
                // propagated `df_conflicts` — then this slot collides with the
                // colliding file/directory in another source. Synthesize git's
                // `o->df_conflict_entry` marker so the merge function resolves
                // the collision to conflict stages instead of a stage-0 entry.
                None if df_conflict_slot(tree, path) => Some(CacheEntry::df_conflict_marker()),
                None => None,
            };
        }

        match merge_fn {
            MergeFn::OneWay => oneway_merge(&src, path, &mut state, opts, probe)?,
            MergeFn::TwoWay => twoway_merge(&src, path, &mut state, opts, probe)?,
            MergeFn::ThreeWay => threeway_merge(&src, path, &mut state, opts, probe)?,
            MergeFn::Bind => bind_merge(&src, path, &mut state, opts, probe)?,
        }
    }

    let UnpackTreesState {
        mut result,
        removals,
        ..
    } = state;
    result.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.entry.stage.cmp(&b.entry.stage))
    });

    let removed_paths = removals.into_iter().map(|r| r.path).collect();
    Ok(UnpackTreesResult {
        entries: result,
        removed_paths,
    })
}

/// git's `check_updates`: apply the merge result to the working tree, then
/// fold the post-write `lstat` data back into the result entries so the
/// consumer serializes a stat-accurate index (git's `state.refresh_cache = 1`).
///
/// Ordering mirrors upstream exactly: every `CE_WT_REMOVE` path is unlinked
/// first (so a path that was a file is gone before a sibling directory is
/// created, and a directory being collapsed into a file is cleared), then each
/// `CE_UPDATE` entry is written. The [`WorktreeWriter`] owns the per-path D/F
/// removal, leading-directory creation, and symlink-vs-regular-file choice; it
/// returns the written file's [`StatInfo`], which is stored into the matching
/// entry here.
///
/// A no-op (other than the early return) when `!opts.update || opts.index_only`.
pub fn check_updates<W: WorktreeWriter>(
    result: &mut UnpackTreesResult,
    opts: &UnpackTreesOptions,
    writer: &mut W,
) -> Result<()> {
    if !opts.update || opts.index_only {
        return Ok(());
    }
    // Removals before writes: git unlinks every CE_WT_REMOVE entry, then
    // checks out the CE_UPDATE entries (so a file→dir or dir→file transition
    // never collides with a stale path).
    for path in &result.removed_paths {
        writer.remove_path(path)?;
    }
    for entry in &mut result.entries {
        if entry.wt_update && entry.entry.stage == 0 {
            let stat = writer.write_blob(&entry.path, entry.entry.mode, &entry.entry.oid)?;
            // git's fill_stat_cache_info: stamp the freshly-written file's
            // lstat onto the entry so a follow-up diff-files reports it clean.
            entry.entry.stat = stat;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_raw(ObjectFormat::Sha1, &[byte; 20]).expect("20-byte sha1 oid")
    }

    fn flat(items: &[(&[u8], u8)]) -> FlatTree {
        items
            .iter()
            .map(|(p, b)| (p.to_vec(), (0o100644u32, oid(*b))))
            .collect()
    }

    /// A `FlatIndex` (path → `(mode, oid, stat)`) with no cached stat info, the
    /// shape the engine sees for a stat-less / freshly-built index input.
    fn idx(items: &[(&[u8], u8)]) -> FlatIndex {
        items
            .iter()
            .map(|(p, b)| (p.to_vec(), (0o100644u32, oid(*b), None)))
            .collect()
    }

    /// A `FlatIndex` whose single entry carries an explicit [`StatInfo`], used
    /// to assert the merge carries cached stat through a kept entry.
    fn idx_with_stat(path: &[u8], byte: u8, stat: StatInfo) -> FlatIndex {
        [(path.to_vec(), (0o100644u32, oid(byte), Some(stat)))]
            .into_iter()
            .collect()
    }

    /// A non-zero sample stat, distinct enough to detect when it is preserved.
    fn sample_stat() -> StatInfo {
        StatInfo {
            ctime_seconds: 111,
            ctime_nanoseconds: 222,
            mtime_seconds: 333,
            mtime_nanoseconds: 444,
            dev: 5,
            ino: 6,
            uid: 7,
            gid: 8,
            size: 9,
        }
    }

    fn opts() -> UnpackTreesOptions {
        UnpackTreesOptions::new(ObjectFormat::Sha1)
    }

    #[test]
    fn sparse_descendants_merge_logically_without_worktree_updates() {
        let mut index = idx(&[(b"outside/changed", 1), (b"outside/deleted", 2)]);
        index.mark_sparse_directory(b"outside/".to_vec());
        let old = flat(&[(b"outside/changed", 1), (b"outside/deleted", 2)]);
        let new = flat(&[(b"outside/changed", 3), (b"outside/added", 4)]);
        let mut options = opts();
        options.update = true;

        let result = unpack_trees(
            &index,
            &[old, new],
            MergeFn::TwoWay,
            &options,
            &NullWorktree,
        )
        .expect("merge collapsed sparse-directory descendants");

        assert_eq!(
            result
                .entries
                .iter()
                .map(|entry| (entry.path.as_slice(), entry.entry.oid, entry.wt_update))
                .collect::<Vec<_>>(),
            [
                (b"outside/added".as_slice(), oid(4), false),
                (b"outside/changed".as_slice(), oid(3), false),
            ]
        );
        assert!(
            result.removed_paths.is_empty(),
            "an absent skip-worktree descendant must not schedule a worktree removal"
        );
    }

    /// A `WorktreeWriter` that records the order of `write_blob` / `remove_path`
    /// calls and hands back a deterministic [`StatInfo`] per written path so the
    /// stat-writeback can be asserted.
    #[derive(Default)]
    struct RecordingWriter {
        ops: Vec<(Vec<u8>, &'static str)>,
        /// `(mode, len)` per written path → used to synthesize a stat.
        next_size: u32,
    }

    impl WorktreeWriter for RecordingWriter {
        fn write_blob(
            &mut self,
            path: &[u8],
            _mode: u32,
            _oid: &ObjectId,
        ) -> Result<Option<StatInfo>> {
            self.ops.push((path.to_vec(), "write"));
            self.next_size += 1;
            Ok(Some(StatInfo {
                size: self.next_size,
                mtime_seconds: 1000 + self.next_size,
                ..StatInfo::default()
            }))
        }
        fn remove_path(&mut self, path: &[u8]) -> Result<()> {
            self.ops.push((path.to_vec(), "remove"));
            Ok(())
        }
    }

    #[test]
    fn munge_size_matches_git() {
        // Ordinary sizes truncate to their low 32 bits.
        assert_eq!(StatInfo::munge_size(0), 0);
        assert_eq!(StatInfo::munge_size(14), 14);
        // A non-zero exact-4GiB multiple (low 32 bits zero) becomes 0x80000000
        // so it isn't read as the racy-smudged "size 0" sentinel.
        assert_eq!(StatInfo::munge_size(1u64 << 32), 0x8000_0000);
        assert_eq!(StatInfo::munge_size(3u64 << 32), 0x8000_0000);
        // A size whose low 32 bits are non-zero keeps them.
        assert_eq!(StatInfo::munge_size((1u64 << 32) + 5), 5);
    }

    #[test]
    fn identical_oneway_entry_carries_cached_stat() {
        // index has `a`@1 with a real stat; tree has `a`@1 (identical) → the
        // kept entry must carry the index's stat (git's oneway same() path).
        let stat = sample_stat();
        let index = idx_with_stat(b"a", 1, stat);
        let tree = flat(&[(b"a", 1)]);
        let res = unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &NullWorktree)
            .expect("oneway identical");
        assert_eq!(res.entries.len(), 1);
        assert_eq!(
            res.entries[0].entry.stat,
            Some(stat),
            "cached stat preserved"
        );
        assert!(
            !res.entries[0].wt_update,
            "identical entry is not rewritten"
        );
    }

    #[test]
    fn identical_merged_entry_carries_cached_stat() {
        // twoway with current==new (an update that resolves to the same content)
        // re-uses the old entry's stat via merged_entry's same() branch.
        let stat = sample_stat();
        // current=a@2 (with stat), oldtree=a@1, newtree=a@2 → merged_entry(new,
        // current) sees same(old=current, merge=new) and copies the stat.
        let index = idx_with_stat(b"a", 2, stat);
        let oldtree = flat(&[(b"a", 1)]);
        let newtree = flat(&[(b"a", 2)]);
        let res = unpack_trees(
            &index,
            &[oldtree, newtree],
            MergeFn::TwoWay,
            &opts(),
            &NullWorktree,
        )
        .expect("twoway same-as-new");
        assert_eq!(res.entries.len(), 1);
        assert_eq!(
            res.entries[0].entry.stat,
            Some(stat),
            "stat carried when merged entry equals the old index entry"
        );
        assert!(!res.entries[0].wt_update);
    }

    #[test]
    fn check_updates_orders_removals_before_writes_and_writes_back_stat() {
        // oneway: index has `a`,`b`; tree has `b`(changed),`c` → `a` removed,
        // `b` rewritten, `c` added. With -u, removals must run before writes and
        // each written entry must get its post-write stat folded back.
        let index = idx(&[(b"a", 1), (b"b", 2)]);
        let tree = flat(&[(b"b", 9), (b"c", 3)]);
        let mut o = opts();
        o.update = true;
        let mut res =
            unpack_trees(&index, &[tree], MergeFn::OneWay, &o, &NullWorktree).expect("oneway -u");
        let mut writer = RecordingWriter::default();
        check_updates(&mut res, &o, &mut writer).expect("apply");
        // The single removal (`a`) runs before any write.
        let first_write = writer
            .ops
            .iter()
            .position(|(_, kind)| *kind == "write")
            .expect("a write happened");
        let last_remove = writer
            .ops
            .iter()
            .rposition(|(_, kind)| *kind == "remove")
            .expect("a remove happened");
        assert!(
            last_remove < first_write,
            "all removals precede all writes: {:?}",
            writer.ops
        );
        // The written entries (`b`,`c`) carry the stat the writer returned.
        for e in &res.entries {
            assert!(
                e.entry.stat.is_some(),
                "written entry {:?} got stat back",
                String::from_utf8_lossy(&e.path)
            );
        }
    }

    #[test]
    fn check_updates_noop_without_update_leaves_stat_untouched() {
        // Without -u, no worktree calls and no stat writeback happen.
        let index = idx(&[(b"a", 1)]);
        let tree = flat(&[(b"a", 9)]);
        let mut res = unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &NullWorktree)
            .expect("oneway no -u");
        let mut writer = RecordingWriter::default();
        check_updates(&mut res, &opts(), &mut writer).expect("apply no-op");
        assert!(writer.ops.is_empty(), "no worktree mutation without -u");
        assert_eq!(res.entries[0].entry.stat, None);
    }

    #[test]
    fn oneway_takes_tree_wholesale() {
        let index = idx(&[(b"a", 1), (b"b", 2)]);
        let tree = flat(&[(b"b", 9), (b"c", 3)]);
        let res = unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &NullWorktree)
            .expect("oneway merge");
        let got: Vec<_> = res
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.entry.oid))
            .collect();
        // `a` deleted (not in tree), `b` updated to tree's, `c` added.
        assert_eq!(got, vec![(b"b".to_vec(), oid(9)), (b"c".to_vec(), oid(3))]);
        assert_eq!(res.removed_paths, vec![b"a".to_vec()]);
    }

    #[test]
    fn twoway_carries_local_addition() {
        // old=a, new=a (unchanged), index has a local addition `local`.
        let index = idx(&[(b"a", 1), (b"local", 5)]);
        let old = flat(&[(b"a", 1)]);
        let new = flat(&[(b"a", 1)]);
        let res = unpack_trees(&index, &[old, new], MergeFn::TwoWay, &opts(), &NullWorktree)
            .expect("twoway merge");
        let paths: Vec<_> = res.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, vec![b"a".to_vec(), b"local".to_vec()]);
    }

    #[test]
    fn checkout_transition_derives_initial_checkout_from_empty_index() {
        let new = flat(&[(b"a", 1)]);
        let plan = plan_checkout_transition(
            &FlatIndex::new(),
            None,
            new,
            CheckoutTransitionOptions::new(ObjectFormat::Sha1),
            &NullWorktree,
        )
        .expect("plan unborn checkout");
        assert_eq!(plan.result().entries.len(), 1);
        assert_eq!(plan.result().entries[0].path, b"a");
        assert!(plan.result().entries[0].wt_update);
    }

    #[test]
    fn checkout_transition_honors_staged_deletion_in_existing_index() {
        let index = idx(&[(b"local", 9)]);
        let old = flat(&[(b"a", 1)]);
        let new = flat(&[(b"a", 1)]);
        let plan = plan_checkout_transition(
            &index,
            Some(old),
            new,
            CheckoutTransitionOptions::new(ObjectFormat::Sha1),
            &NullWorktree,
        )
        .expect("plan checkout with staged deletion");
        let paths = plan
            .result()
            .entries
            .iter()
            .map(|entry| entry.path.as_slice())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![b"local".as_slice()]);
    }

    #[test]
    fn checkout_transition_apply_refreshes_written_entry_stat() {
        let index = idx(&[(b"a", 1)]);
        let old = flat(&[(b"a", 1)]);
        let new = flat(&[(b"a", 2)]);
        let plan = plan_checkout_transition(
            &index,
            Some(old),
            new,
            CheckoutTransitionOptions::new(ObjectFormat::Sha1),
            &NullWorktree,
        )
        .expect("plan checkout update");
        let mut writer = RecordingWriter::default();
        let result = plan.apply(&mut writer).expect("apply checkout update");
        assert_eq!(writer.ops, vec![(b"a".to_vec(), "write")]);
        assert_eq!(
            result.entries[0].entry.stat,
            Some(StatInfo {
                size: 1,
                mtime_seconds: 1001,
                ..StatInfo::default()
            })
        );
    }

    #[test]
    fn threeway_resolves_when_one_side_unchanged() {
        // base=a@1, ours=a@2 (changed), theirs=a@1 (unchanged) → take ours.
        let index = idx(&[(b"a", 2)]);
        let base = flat(&[(b"a", 1)]);
        let ours = flat(&[(b"a", 2)]);
        let theirs = flat(&[(b"a", 1)]);
        let res = unpack_trees(
            &index,
            &[base, ours, theirs],
            MergeFn::ThreeWay,
            &opts(),
            &NullWorktree,
        )
        .expect("threeway merge");
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].entry.oid, oid(2));
        assert_eq!(res.entries[0].entry.stage, 0);
        assert!(!has_conflict(&res));
    }

    #[test]
    fn threeway_conflict_emits_stages() {
        // base=a@1, ours=a@2, theirs=a@3 — all differ → stages 1/2/3.
        let index = idx(&[(b"a", 2)]);
        let base = flat(&[(b"a", 1)]);
        let ours = flat(&[(b"a", 2)]);
        let theirs = flat(&[(b"a", 3)]);
        let res = unpack_trees(
            &index,
            &[base, ours, theirs],
            MergeFn::ThreeWay,
            &opts(),
            &NullWorktree,
        )
        .expect("threeway conflict");
        let stages: Vec<u8> = res.entries.iter().map(|e| e.entry.stage).collect();
        assert_eq!(stages, vec![1, 2, 3]);
    }

    #[test]
    fn threeway_multi_ancestor_16_keeps_head_and_remote_only() {
        // t1000 case #16: one ancestor matches ours and another matches theirs.
        // This suppresses the #13/#14 trivial-resolution arms and records only
        // the real head/remote sides as conflict stages.
        let index = idx(&[(b"F16", 2)]);
        let ancestor_remote = flat(&[(b"F16", 1)]);
        let ancestor_head = flat(&[(b"F16", 2)]);
        let head = flat(&[(b"F16", 2)]);
        let remote = flat(&[(b"F16", 1)]);
        let res = unpack_trees(
            &index,
            &[ancestor_remote, ancestor_head, head, remote],
            MergeFn::ThreeWay,
            &opts(),
            &NullWorktree,
        )
        .expect("multi-ancestor threeway");
        let got: Vec<_> = res
            .entries
            .iter()
            .map(|e| (e.entry.stage, e.entry.oid))
            .collect();
        assert_eq!(got, vec![(2, oid(2)), (3, oid(1))]);
    }

    /// `(stage, path)` pairs for the result, sorted as `git ls-files -s` emits.
    fn staged_paths(res: &UnpackTreesResult) -> Vec<(u8, Vec<u8>)> {
        res.entries
            .iter()
            .map(|e| (e.entry.stage, e.path.clone()))
            .collect()
    }

    #[test]
    fn df_conflict_slot_detects_directory_and_file_ancestor() {
        // `a/b` is a directory (has `a/b/c/d`) → marker for a file at `a/b`.
        let tree = flat(&[(b"a/b/c/d", 1)]);
        assert!(df_conflict_slot(&tree, b"a/b"));
        // `a/b` is a file → `a/b/c/d` lives under it → marker for that slot.
        let tree2 = flat(&[(b"a/b", 1)]);
        assert!(df_conflict_slot(&tree2, b"a/b/c/d"));
        // Recursive: a deep ancestor file poisons a deeper descendant.
        assert!(df_conflict_slot(&tree2, b"a/b/c/d/e/f"));
        // No collision: `a/b-2/...` is a sibling, not under `a/b/`.
        let tree3 = flat(&[(b"a/b-2/c/d", 1)]);
        assert!(!df_conflict_slot(&tree3, b"a/b"));
        // A `.c` suffix is not a directory prefix of `ioat/`.
        let tree4 = flat(&[(b"ds/dma/ioat/Makefile", 1)]);
        assert!(!df_conflict_slot(&tree4, b"ds/dma/ioat.c"));
    }

    #[test]
    fn threeway_df_conflict_synthesizes_stages() {
        // git t1012 "3-way (1)": O,A have `a/b/c/d` (a/b is a directory); B has
        // `a/b` (a file). Expected: `3 a/b`, `1 a/b/c/d`, `2 a/b/c/d`.
        let index = idx(&[(b"a/b/c/d", 2), (b"a/b-2/c/d", 2), (b"a/x", 2)]);
        let base = flat(&[(b"a/b/c/d", 1), (b"a/b-2/c/d", 1), (b"a/x", 1)]);
        let ours = flat(&[(b"a/b/c/d", 2), (b"a/b-2/c/d", 2), (b"a/x", 2)]);
        let theirs = flat(&[(b"a/b", 3), (b"a/b-2/c/d", 2), (b"a/x", 2)]);
        let res = unpack_trees(
            &index,
            &[base, ours, theirs],
            MergeFn::ThreeWay,
            &opts(),
            &NullWorktree,
        )
        .expect("threeway D/F");
        assert_eq!(
            staged_paths(&res),
            vec![
                (3, b"a/b".to_vec()),
                (0, b"a/b-2/c/d".to_vec()),
                (1, b"a/b/c/d".to_vec()),
                (2, b"a/b/c/d".to_vec()),
                (0, b"a/x".to_vec()),
            ],
            "the file `a/b` lands at stage 3, the dir-side `a/b/c/d` at stages 1+2"
        );
    }

    #[test]
    fn threeway_df_conflict_recursive_three_levels() {
        // git t1004 "D/F": branch-point + ours keep `subdir/file2` as a FILE;
        // theirs turns it into a directory `subdir/file2/another`. Expected
        // unmerged stages: `1 subdir/file2`, `2 subdir/file2`,
        // `3 subdir/file2/another` — a 3-level recursive D/F. The index is set
        // to ours (a `settree side-b` precedes the read-tree), so the index
        // matches head.
        let index = idx(&[(b"subdir/file2", 1)]);
        let base = flat(&[(b"subdir/file2", 1)]);
        let ours = flat(&[(b"subdir/file2", 1)]); // same as base → file kept
        let theirs = flat(&[(b"subdir/file2/another", 3)]);
        let res = unpack_trees(
            &index,
            &[base, ours, theirs],
            MergeFn::ThreeWay,
            &opts(),
            &NullWorktree,
        )
        .expect("threeway recursive D/F");
        assert_eq!(
            staged_paths(&res),
            vec![
                (1, b"subdir/file2".to_vec()),
                (2, b"subdir/file2".to_vec()),
                (3, b"subdir/file2/another".to_vec()),
            ]
        );
    }

    #[test]
    fn twoway_df_marker_treated_as_absent() {
        // twoway: index+oldtree have `a/b` (a file); newtree turns it into a
        // directory `a/b/c`. The marker on the newtree slot for `a/b` is
        // treated as absent (no real file) → the file is deleted and `a/b/c`
        // is added. No stale `a/b` survives.
        let index = idx(&[(b"a/b", 1)]);
        let old = flat(&[(b"a/b", 1)]);
        let new = flat(&[(b"a/b/c", 9)]);
        let res = unpack_trees(&index, &[old, new], MergeFn::TwoWay, &opts(), &NullWorktree)
            .expect("twoway D/F");
        assert_eq!(staged_paths(&res), vec![(0, b"a/b/c".to_vec())]);
        assert_eq!(res.removed_paths, vec![b"a/b".to_vec()]);
    }

    /// Whether any path produced conflict stages (a stage > 0 entry).
    fn has_conflict(res: &UnpackTreesResult) -> bool {
        res.entries.iter().any(|e| e.entry.stage != 0)
    }

    // ----- submodule move-head hook --------------------------------------

    /// Raw git mode for a gitlink (submodule) cache entry.
    const GITLINK_MODE: u32 = 0o160000;

    /// One recorded `check_submodule_move_head` invocation:
    /// `(path, old_oid, new_oid, reset)`.
    type MoveHeadCall = (Vec<u8>, Option<ObjectId>, ObjectId, ResetType);

    /// A probe that records every `check_submodule_move_head` call and, when
    /// `reject_path` is set, returns git's `ERROR_WOULD_LOSE_SUBMODULE`-style
    /// error for that path — letting a test assert both *that* the engine
    /// invoked the hook (and with which args) and that the rejection
    /// propagates out of `unpack_trees`.
    #[derive(Default)]
    struct RecordingProbe {
        calls: std::cell::RefCell<Vec<MoveHeadCall>>,
        reject_path: Option<Vec<u8>>,
    }

    impl WorktreeProbe for RecordingProbe {
        fn verify_uptodate(&self, _path: &[u8], _ce: &CacheEntry) -> Result<()> {
            Ok(())
        }
        fn verify_absent_overwrite(
            &self,
            _path: &[u8],
            _merge: &CacheEntry,
            _reset: ResetType,
        ) -> Result<()> {
            Ok(())
        }
        fn verify_absent_remove(&self, _path: &[u8], _reset: ResetType) -> Result<()> {
            Ok(())
        }
        fn check_submodule_move_head(
            &self,
            path: &[u8],
            old_oid: Option<&ObjectId>,
            new_oid: &ObjectId,
            reset: ResetType,
        ) -> Result<()> {
            self.calls
                .borrow_mut()
                .push((path.to_vec(), old_oid.copied(), *new_oid, reset));
            if self.reject_path.as_deref() == Some(path) {
                return Err(GitError::Exit(128));
            }
            Ok(())
        }
    }

    /// A gitlink (mode 0o160000) entry flat map.
    fn gitlink(items: &[(&[u8], u8)]) -> FlatTree {
        items
            .iter()
            .map(|(p, b)| (p.to_vec(), (GITLINK_MODE, oid(*b))))
            .collect()
    }

    /// A gitlink `FlatIndex` (path → `(mode, oid, stat)`, no cached stat).
    fn gitlink_idx(items: &[(&[u8], u8)]) -> FlatIndex {
        items
            .iter()
            .map(|(p, b)| (p.to_vec(), (GITLINK_MODE, oid(*b), None)))
            .collect()
    }

    #[test]
    fn move_head_hook_fires_for_new_gitlink() {
        // Empty index, tree adds a gitlink `sub` → the None arm of merged_entry
        // must call check_submodule_move_head with old == None.
        let index = idx(&[]);
        let tree = gitlink(&[(b"sub", 7)]);
        let probe = RecordingProbe::default();
        let mut opts = opts();
        opts.update = true; // exercise the -u worktree path too
        unpack_trees(&index, &[tree], MergeFn::OneWay, &opts, &probe).expect("oneway add gitlink");
        let calls = probe.calls.borrow();
        assert_eq!(calls.len(), 1, "exactly one move-head call");
        assert_eq!(calls[0].0, b"sub".to_vec());
        assert_eq!(calls[0].1, None, "old_oid is None for a new submodule");
        assert_eq!(calls[0].2, oid(7), "new_oid is the tree's gitlink oid");
    }

    #[test]
    fn move_head_hook_fires_for_changed_gitlink() {
        // Index has gitlink `sub`@1, tree moves it to `sub`@2 → the Some(old)
        // arm must call the hook with old == Some(1), new == 2.
        let index = gitlink_idx(&[(b"sub", 1)]);
        let tree = gitlink(&[(b"sub", 2)]);
        let probe = RecordingProbe::default();
        unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &probe)
            .expect("oneway move gitlink head");
        let calls = probe.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, b"sub".to_vec());
        assert_eq!(
            calls[0].1,
            Some(oid(1)),
            "old_oid is the prior gitlink commit"
        );
        assert_eq!(calls[0].2, oid(2), "new_oid is the new gitlink commit");
    }

    #[test]
    fn move_head_hook_not_fired_for_regular_file() {
        // A plain blob change must NOT trigger the submodule hook.
        let index = idx(&[(b"a", 1)]);
        let tree = flat(&[(b"a", 2)]);
        let probe = RecordingProbe::default();
        unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &probe).expect("oneway blob");
        assert!(
            probe.calls.borrow().is_empty(),
            "non-gitlink entries skip the move-head hook"
        );
    }

    #[test]
    fn move_head_hook_not_fired_for_identical_gitlink() {
        // Unchanged gitlink: oneway_merge's `same` fast-path keeps the entry
        // without entering merged_entry, so the hook is not called (git only
        // runs check_submodule_move_head inside merged_entry).
        let index = gitlink_idx(&[(b"sub", 5)]);
        let tree = gitlink(&[(b"sub", 5)]);
        let probe = RecordingProbe::default();
        unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &probe)
            .expect("oneway same gitlink");
        assert!(
            probe.calls.borrow().is_empty(),
            "an unchanged gitlink never reaches merged_entry"
        );
    }

    #[test]
    fn move_head_rejection_propagates() {
        // A WouldLose verdict (probe returns Err) must abort the whole run.
        let index = gitlink_idx(&[(b"sub", 1)]);
        let tree = gitlink(&[(b"sub", 2)]);
        let probe = RecordingProbe {
            reject_path: Some(b"sub".to_vec()),
            ..RecordingProbe::default()
        };
        let err = unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &probe)
            .expect_err("a would-lose-submodule rejection aborts the merge");
        assert!(matches!(err, GitError::Exit(128)));
    }

    #[test]
    fn move_head_hook_carries_reset_flag() {
        // `--reset` (OverwriteUntracked) must reach the probe so it can map to
        // the FORCE flag of check_submodule_move_head.
        let index = gitlink_idx(&[(b"sub", 1)]);
        let tree = gitlink(&[(b"sub", 2)]);
        let probe = RecordingProbe::default();
        let mut opts = opts();
        opts.reset = ResetType::OverwriteUntracked;
        unpack_trees(&index, &[tree], MergeFn::OneWay, &opts, &probe)
            .expect("oneway reset gitlink");
        assert_eq!(
            probe.calls.borrow()[0].3,
            ResetType::OverwriteUntracked,
            "reset type is forwarded to the move-head hook"
        );
    }

    #[test]
    fn null_worktree_default_hook_is_noop() {
        // NullWorktree uses the trait default (Ok) for the new method, so a
        // gitlink move through it succeeds silently — index-only consumers are
        // unchanged.
        let index = gitlink_idx(&[(b"sub", 1)]);
        let tree = gitlink(&[(b"sub", 2)]);
        let res = unpack_trees(&index, &[tree], MergeFn::OneWay, &opts(), &NullWorktree)
            .expect("NullWorktree default hook is a no-op");
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].entry.oid, oid(2));
    }
}
