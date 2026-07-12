//! Pack-backed object storage: [`FileObjectDatabase`], in-memory [`ObjectDatabase`],
//! and the shared decode/index caches that back packed reads.
//!
//! ## Cache invariants (thread safety)
//!
//! A [`FileObjectDatabase`] is [`Clone`] via `Arc` on every cache map. Cloned handles
//! share the same caches and may be used from multiple threads concurrently.
//!
//! * **Read-mostly maps** (`pack_indexes`, `pack_bytes`, `pack_reverse_indexes`,
//!   `multi_pack_indexes`, `multi_pack_oid_lookups`) use [`RwLock`]: concurrent readers
//!   take shared locks; writers take exclusive locks only on insert/clear. Lookups never
//!   hold a lock across decode or I/O.
//! * **Mutation-heavy maps** (`decoded`, `pack_deltas`, `pack_header_types`, `pack_registry`)
//!   stay behind [`Mutex`] because inserts and LRU eviction interleave reads and writes on
//!   the same critical section.
//! * **Per-pack state** on [`RegisteredPack`](crate::registry::RegisteredPack) mirrors the
//!   split: parsed indexes are [`RwLock`]-cached; delta-base LRU caches stay [`Mutex`]-backed.
//! * **`refresh_read_cache`** clears every shared map so the next read sees packs installed
//!   out-of-band; callers must not hold a cache guard across it.

use parking_lot::RwLock;
use sley_core::{GitError, MissingObjectContext, ObjectFormat, ObjectId, Result};
use sley_object::{Commit, EncodedObject, ObjectType, Tag, TreeEntries};
use sley_pack::{
    MultiPackIndex, MultiPackIndexOidLookup, PackIndex, PackIndexByteSource, PackIndexViewData,
    PackReverseIndex,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use crate::{ObjectReader, ObjectWriter, implied_empty_tree_object};

use crate::install::{ObjectPrefixResolution, ObjectStorageInfo};
use crate::loose::LooseObjectStore;
use crate::reachability::{
    PackDeltaBase, PackIndexOffsetInfo, pack_entry_delta_base, scan_pack_index_offsets,
    scan_pack_offsets_without_index, zero_oid,
};
use crate::registry::{
    ObjectPresenceChecker, PackLookup, PackRegistryCache, PackRegistrySnapshot,
    alternate_object_dirs, object_ids_in_objects_dir, object_ids_with_prefix_in_objects_dir,
    read_incremental_midx_chain, repository_objects_dir, same_registered_pack_set,
    scan_pack_registry, validate_object_id_prefix,
};

pub struct ObjectDatabase {
    pub(crate) format: ObjectFormat,
    // Behind a `Mutex` so `write_object` can take `&self` (matching the
    // `ObjectWriter` trait) and a single handle can interleave reads and writes
    // without a `&mut` borrow — the same shared-by-`&` shape the file-backed
    // database uses for its caches. Removes the need for callers to wrap this in
    // a `RefCell`/`&mut` just to write (see sley-fetch's former `RefCell` dance).
    pub(crate) objects: Mutex<HashMap<ObjectId, Arc<EncodedObject>>>,
    pub(crate) promisor: bool,
}

impl ObjectDatabase {
    pub fn new(format: ObjectFormat) -> Self {
        Self {
            format,
            objects: Mutex::new(HashMap::new()),
            promisor: false,
        }
    }

    pub fn with_promisor(mut self, promisor: bool) -> Self {
        self.promisor = promisor;
        self
    }

    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.objects
            .lock()
            .map(|objects| objects.contains_key(oid))
            .unwrap_or(false)
    }

    pub fn validate(&self, oid: &ObjectId) -> Result<()> {
        let object = self.read_object(oid)?;
        let actual = object.object_id(self.format)?;
        if &actual == oid {
            Ok(())
        } else {
            Err(GitError::InvalidObject(format!(
                "object id mismatch: expected {oid}, got {actual}"
            )))
        }
    }
}

impl ObjectReader for ObjectDatabase {
    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.objects
            .lock()
            .map_err(|_| GitError::object_not_found_in(*oid, MissingObjectContext::Read))?
            .get(oid)
            .map(Arc::clone)
            .or_else(|| implied_empty_tree_object(self.format, oid))
            .ok_or_else(|| GitError::object_not_found_in(*oid, MissingObjectContext::Read))
    }
}

impl ObjectWriter for ObjectDatabase {
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        let oid = object.object_id(self.format)?;
        self.objects
            .lock()
            .map_err(|_| GitError::Io("object cache lock poisoned".into()))?
            .entry(oid)
            .or_insert_with(|| Arc::new(object));
        Ok(oid)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternate {
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialClonePolicy {
    pub promisor_remote: Option<String>,
    pub allow_missing_promised_objects: bool,
}

/// Raw pack-file bytes keyed by pack path, shared across cloned handles. Loaded
/// once so individual objects can be decoded at their offsets (see
/// [`sley_pack::read_object_at`]) without re-reading the whole file per read.
pub(crate) type PackBytesCache = Arc<RwLock<HashMap<PathBuf, Arc<PackData>>>>;

/// Backing bytes of a pack file: either memory-mapped (under the `mmap` feature)
/// or read into the heap. Both deref to `&[u8]`, so the decode path is identical.
#[derive(Debug)]
pub(crate) enum PackData {
    #[cfg(feature = "mmap")]
    Mapped(sley_mmap::MappedFile),
    Heap(Vec<u8>),
}

impl std::ops::Deref for PackData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(feature = "mmap")]
            Self::Mapped(mapped) => mapped,
            Self::Heap(bytes) => bytes,
        }
    }
}

/// Load a pack file's bytes: memory-mapped when the `mmap` feature is on (falling
/// back to a heap read if the map fails), otherwise read into the heap.
#[cfg(feature = "mmap")]
pub(crate) fn load_pack_data(pack_path: &Path) -> Result<PackData> {
    match sley_mmap::MappedFile::open_pack(pack_path) {
        Ok(mapped) => Ok(PackData::Mapped(mapped)),
        Err(_) => Ok(PackData::Heap(fs::read(pack_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
pub(crate) fn load_pack_data(pack_path: &Path) -> Result<PackData> {
    Ok(PackData::Heap(fs::read(pack_path)?))
}

#[cfg(feature = "mmap")]
pub(crate) fn load_pack_index_data(index_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    match sley_mmap::MappedFile::open_pack(index_path) {
        Ok(mapped) => Ok(Arc::new(mapped)),
        Err(_) => Ok(Arc::new(fs::read(index_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
pub(crate) fn load_pack_index_data(index_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    Ok(Arc::new(fs::read(index_path)?))
}

#[cfg(feature = "mmap")]
fn load_multi_pack_index_lookup_data(midx_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    match sley_mmap::MappedFile::open_multi_pack_index(midx_path) {
        Ok(mapped) => Ok(Arc::new(mapped)),
        Err(_) => Ok(Arc::new(fs::read(midx_path)?)),
    }
}

#[cfg(not(feature = "mmap"))]
fn load_multi_pack_index_lookup_data(midx_path: &Path) -> Result<Arc<dyn PackIndexByteSource>> {
    Ok(Arc::new(fs::read(midx_path)?))
}

/// Memory-capped LRU of recently decoded objects, shared across cloned handles,
/// so hot delta bases and repeated reads during a walk aren't re-decoded. The
/// cache is bounded by an approximate byte budget (not a fixed object count) so
/// it neither thrashes on bulk reads of small objects nor blows up on a few
/// large ones.
pub(crate) type DecodedObjectCache = Arc<Mutex<LruObjectCache>>;

/// Per-pack caches of objects decoded from a pack, keyed by pack path and then by
/// the in-pack byte offset of each object's entry. Shared across cloned handles.
/// This is the delta-base cache: resolving a delta chain by offset reuses already
/// decoded bases instead of re-inflating the whole chain on every read.
pub(crate) type PackDeltaCaches = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<LruOffsetCache>>>>>;

/// Per-pack memo of `in-pack offset -> end-of-chain object type` for the
/// `cat-file --batch-check` header fast path. Resolving a packed delta's *type*
/// walks the delta chain to its base; without this memo every header read
/// re-walks (and re-inflates) the whole chain, so reading every object in a
/// deeply-deltified pack is super-linear (sley#26). The type only depends on the
/// chain base, so memoizing `offset -> type` lets each chain be walked at most
/// once across a batch. Keyed by pack path so an offset key is never applied to
/// the wrong pack's bytes; shared across cloned handles.
/// One pack's offset-keyed header memo (see [`PackHeaderTypeCaches`]).
pub(crate) type PackHeaderTypeCache = Arc<Mutex<HashMap<u64, (ObjectType, u64)>>>;

pub(crate) type PackHeaderTypeCaches = Arc<Mutex<HashMap<PathBuf, PackHeaderTypeCache>>>;

/// Default approximate byte budget for the decoded-object LRU. Sized to comfortably
/// hold the working set of a history walk (commits/trees/blobs and their delta
/// bases) without growing without bound on large repositories. Overridable via the
/// `SLEY_OBJECT_CACHE_BYTES` environment variable; there is currently no git-config
/// hook threaded into the object database, so this constant is the default.
const DEFAULT_OBJECT_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// Default approximate byte budget for each per-pack delta-base cache. Holds the
/// decoded bases of the delta chains being walked so neighboring reads stay warm.
/// Overridable via `SLEY_DELTA_BASE_CACHE_BYTES`.
const DEFAULT_DELTA_BASE_CACHE_BYTES: usize = 96 * 1024 * 1024;

/// Approximate heap cost of caching one [`EncodedObject`]: its body plus a fixed
/// allowance for the key, enum/`Vec` headers, and per-entry map overhead. Used
/// only to drive eviction, so an estimate is fine.
pub(crate) fn cached_object_cost(object: &EncodedObject) -> usize {
    object.body.len().saturating_add(64)
}

/// Read an approximate byte budget from `var`, falling back to `default` when the
/// variable is unset or unparseable. A value of `0` disables the cache.
fn cache_budget_from_env(var: &str, default: usize) -> usize {
    match env::var(var) {
        Ok(value) => value.trim().parse::<usize>().unwrap_or(default),
        Err(_) => default,
    }
}

/// Approximate byte budget for the decoded-object LRU (see
/// [`DEFAULT_OBJECT_CACHE_BYTES`], `SLEY_OBJECT_CACHE_BYTES`).
///
/// Resolved once per process: the environment does not change under us, and a new
/// `FileObjectDatabase` is built often enough (e.g. once per revision resolved)
/// that re-reading the variable each time showed up as per-object overhead.
pub(crate) fn object_cache_budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        cache_budget_from_env("SLEY_OBJECT_CACHE_BYTES", DEFAULT_OBJECT_CACHE_BYTES)
    })
}

/// Approximate byte budget for each per-pack delta-base cache (see
/// [`DEFAULT_DELTA_BASE_CACHE_BYTES`], `SLEY_DELTA_BASE_CACHE_BYTES`). Resolved
/// once per process for the same reason as [`object_cache_budget`].
pub(crate) fn delta_base_cache_budget() -> usize {
    static BUDGET: OnceLock<usize> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        cache_budget_from_env(
            "SLEY_DELTA_BASE_CACHE_BYTES",
            DEFAULT_DELTA_BASE_CACHE_BYTES,
        )
    })
}

/// Whether to re-hash every object on read and compare it to the requested id.
///
/// Off by default, matching git: reads trust the pack index → offset mapping and
/// the loose object's on-disk name, and object ids are verified where git verifies
/// them — when a pack is received (the index build re-hashes every object) and on
/// demand via [`FileObjectDatabase`]'s `validate`/fsck. Re-hashing on *every* read
/// dominated bulk-read cost (a scalar pure-Rust SHA-1 over each object's full
/// body), so it is opt-in via `SLEY_VERIFY_READS` (any value other than unset, ``,
/// or `0`) for callers that want the paranoid check back. Read once and cached, so
/// the default path pays only a single relaxed atomic load per read.
pub(crate) fn verify_reads_enabled() -> bool {
    static VERIFY: OnceLock<bool> = OnceLock::new();
    *VERIFY.get_or_init(|| match env::var("SLEY_VERIFY_READS") {
        Ok(value) => !matches!(value.trim(), "" | "0"),
        Err(_) => false,
    })
}

/// A memory-capped LRU map from a key `K` to a decoded [`EncodedObject`].
///
/// Eviction is by approximate byte budget (gix-style), not object count, so the
/// cache adapts to object size. On access an entry is moved to most-recently-used;
/// on insert, least-recently-used entries are dropped until the budget holds. A
/// budget of `0` makes the cache inert. Generic over the key so it backs both the
/// oid-keyed decoded-object cache and the offset-keyed delta-base cache.
#[derive(Debug)]
pub(crate) struct LruCache<K: std::hash::Hash + Eq + Clone> {
    budget: usize,
    used: usize,
    map: HashMap<K, LruEntry<K>>,
    head: Option<K>,
    tail: Option<K>,
}

#[derive(Debug)]
struct LruEntry<K> {
    object: Arc<EncodedObject>,
    prev: Option<K>,
    next: Option<K>,
}

impl<K: std::hash::Hash + Eq + Clone> LruCache<K> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            map: HashMap::new(),
            head: None,
            tail: None,
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<Arc<EncodedObject>> {
        let object = Arc::clone(&self.map.get(key)?.object);
        self.touch(key);
        Some(object)
    }

    /// Move `key` to the most-recently-used end in O(1).
    pub(crate) fn touch(&mut self, key: &K) {
        if self.tail.as_ref() == Some(key) {
            return;
        }
        if self.map.contains_key(key) {
            self.detach(key);
            self.attach_back(key.clone());
        }
    }

    /// Drop `key` from both the map and the recency queue, releasing its budget.
    pub(crate) fn remove(&mut self, key: &K) {
        if let Some(entry) = self.map.get(key) {
            self.used = self.used.saturating_sub(cached_object_cost(&entry.object));
        }
        self.detach(key);
        self.map.remove(key);
    }

    pub(crate) fn detach(&mut self, key: &K) {
        let Some((prev, next)) = self.map.get_mut(key).map(|entry| {
            let prev = entry.prev.take();
            let next = entry.next.take();
            (prev, next)
        }) else {
            return;
        };

        match &prev {
            Some(prev_key) => {
                if let Some(prev_entry) = self.map.get_mut(prev_key) {
                    prev_entry.next = next.clone();
                }
            }
            None => self.head = next.clone(),
        }
        match &next {
            Some(next_key) => {
                if let Some(next_entry) = self.map.get_mut(next_key) {
                    next_entry.prev = prev.clone();
                }
            }
            None => self.tail = prev.clone(),
        }
    }

    pub(crate) fn attach_back(&mut self, key: K) {
        let previous_tail = self.tail.replace(key.clone());
        match previous_tail {
            Some(tail_key) => {
                if let Some(tail_entry) = self.map.get_mut(&tail_key) {
                    tail_entry.next = Some(key.clone());
                }
                if let Some(entry) = self.map.get_mut(&key) {
                    entry.prev = Some(tail_key);
                    entry.next = None;
                }
            }
            None => {
                self.head = Some(key.clone());
                if let Some(entry) = self.map.get_mut(&key) {
                    entry.prev = None;
                    entry.next = None;
                }
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.head = None;
        self.tail = None;
        self.used = 0;
    }

    pub(crate) fn put(&mut self, key: K, object: Arc<EncodedObject>) {
        if self.budget == 0 {
            return;
        }
        let cost = cached_object_cost(&object);
        // A single object larger than the whole budget is not worth caching; it
        // would immediately evict everything including itself. Drop any stale
        // smaller entry stored under the same key so accounting stays exact.
        if cost > self.budget {
            self.remove(&key);
            return;
        }
        if let Some(entry) = self.map.get_mut(&key) {
            let previous = std::mem::replace(&mut entry.object, object);
            // Replacing an existing entry: adjust accounting and refresh recency.
            self.used = self
                .used
                .saturating_sub(cached_object_cost(&previous))
                .saturating_add(cost);
            self.touch(&key);
        } else {
            self.used = self.used.saturating_add(cost);
            self.map.insert(
                key.clone(),
                LruEntry {
                    object,
                    prev: None,
                    next: None,
                },
            );
            self.attach_back(key);
        }
        while self.used > self.budget {
            let Some(evicted) = self.head.clone() else {
                break;
            };
            self.remove(&evicted);
        }
    }
}

/// Decoded-object cache keyed by object id (loose + packed reads share it).
type LruObjectCache = LruCache<ObjectId>;
/// Delta-base cache keyed by in-pack byte offset, scoped to one pack.
pub(crate) type LruOffsetCache = LruCache<u64>;

/// Bridges the offset-keyed [`LruOffsetCache`] to [`sley_pack::PackDeltaCache`]
/// so the pack decoder can reuse decoded delta bases. Holds the shared cache
/// behind its mutex; a poisoned lock simply behaves as a cache miss/no-op, so a
/// decode still completes correctly (just without reuse).
struct PackDeltaCacheAdapter<'a>(&'a Arc<Mutex<LruOffsetCache>>);

impl sley_pack::PackDeltaCache for PackDeltaCacheAdapter<'_> {
    fn get(&self, offset: u64) -> Option<Arc<EncodedObject>> {
        self.0.lock().ok()?.get(&offset)
    }

    fn insert(&self, offset: u64, object: Arc<EncodedObject>) {
        if let Ok(mut cache) = self.0.lock() {
            cache.put(offset, object);
        }
    }
}

/// Bridges a per-pack `offset -> ObjectType` memo into the header fast path so
/// the ofs-delta chain walk is performed at most once per chain across a batch
/// of `read_object_header` calls (sley#26).
struct PackHeaderTypeCacheAdapter<'a>(&'a PackHeaderTypeCache);

impl sley_pack::HeaderTypeCache for PackHeaderTypeCacheAdapter<'_> {
    fn get(&self, pack_offset: u64) -> Option<(ObjectType, u64)> {
        self.0.lock().ok()?.get(&pack_offset).copied()
    }

    fn put(&mut self, pack_offset: u64, header: (ObjectType, u64)) {
        if let Ok(mut cache) = self.0.lock() {
            cache.insert(pack_offset, header);
        }
    }
}

/// Parsed pack indexes keyed by `.idx` path, shared across cloned handles. This
/// remains for MIDX and path-only fallback lookups; normal pack-directory scans
/// use [`PackRegistrySnapshot`] so the lookup hot path can walk already-parsed
/// pack records directly.
pub(crate) type PackIndexCache = Arc<RwLock<HashMap<PathBuf, Arc<PackIndexViewData>>>>;

/// Optional `.rev` reverse indexes keyed by `.idx` path, shared across cloned
/// handles. A cached `None` means no usable reverse index was found.
pub(crate) type PackReverseIndexCache =
    Arc<RwLock<HashMap<PathBuf, Option<Arc<PackReverseIndex>>>>>;

/// Parsed multi-pack-index files keyed by path, shared across cloned handles.
/// Caches the MIDX parse so object lookups in repositories with a MIDX avoid
/// reparsing the same fanout/object tables for every read.
pub(crate) type MultiPackIndexCache = Arc<RwLock<HashMap<PathBuf, Arc<MultiPackIndex>>>>;

/// Explicit replacement-object policy injected by repository setup.
///
/// The map is deliberately independent of refs/config/environment: embedders
/// decide whether replacement refs are enabled and pass the already-resolved
/// old-to-new object ids into the ODB.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectReplacements {
    replacements: HashMap<ObjectId, ObjectId>,
}

impl ObjectReplacements {
    pub fn new(replacements: impl IntoIterator<Item = (ObjectId, ObjectId)>) -> Self {
        Self {
            replacements: replacements.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.replacements.is_empty()
    }

    /// Follow a replacement chain, rejecting cycles or chains beyond Git's
    /// five-link recursion guard.
    pub fn resolve(&self, oid: &ObjectId) -> Result<ObjectId> {
        let mut current = *oid;
        let mut seen = HashSet::new();
        for _ in 0..5 {
            if !seen.insert(current) {
                return Err(GitError::InvalidObject(format!(
                    "replace depth too high for object {oid}"
                )));
            }
            let Some(next) = self.replacements.get(&current) else {
                return Ok(current);
            };
            // `git replace --graft` may materialize an unchanged commit and
            // consequently write an identity replace ref. Git treats that as
            // a terminal no-op rather than replacement recursion.
            if *next == current {
                return Ok(current);
            }
            current = *next;
        }
        if self.replacements.contains_key(&current) {
            return Err(GitError::InvalidObject(format!(
                "replace depth too high for object {oid}"
            )));
        }
        Ok(current)
    }
}

/// Raw multi-pack-index OID lookup tables keyed by path, shared across cloned
/// handles. These avoid hashing and materializing every MIDX object when a
/// command only needs point lookups.
type MultiPackIndexOidLookupCache = Arc<RwLock<HashMap<PathBuf, Arc<MultiPackIndexOidLookup>>>>;

/// One registered `.idx`/`.pack` pair from a pack directory. The index is parsed
/// when the registry snapshot is built; pack bytes and per-pack decode/header
/// caches hang directly off this record so repeated object lookups do not bounce
/// through path-keyed maps.
#[derive(Debug, Clone)]
pub struct FileObjectDatabase {
    pub(crate) loose: LooseObjectStore,
    pub(crate) objects_dir: PathBuf,
    pub(crate) alternates: Vec<PathBuf>,
    pub(crate) format: ObjectFormat,
    pub(crate) pack_bytes: PackBytesCache,
    pub(crate) pack_indexes: PackIndexCache,
    pub(crate) pack_reverse_indexes: PackReverseIndexCache,
    pub(crate) multi_pack_indexes: MultiPackIndexCache,
    pub(crate) multi_pack_oid_lookups: MultiPackIndexOidLookupCache,
    pub(crate) pack_registry: PackRegistryCache,
    pub(crate) decoded: DecodedObjectCache,
    pub(crate) pack_deltas: PackDeltaCaches,
    pub(crate) pack_header_types: PackHeaderTypeCaches,
    pub(crate) promisor_objects: Arc<OnceLock<HashSet<ObjectId>>>,
    /// Whether the owning repository actually has a promisor remote configured
    /// (`extensions.partialclone` is set, or some `remote.<name>.promisor` is
    /// true). Mirrors git's `is_promisor_object`, which only treats objects in
    /// `.promisor` packs as "promised" when `repo_has_promisor_remote()` holds:
    /// a stray `.promisor` sidecar in a non-partial repo must NOT excuse missing
    /// objects from fsck. Defaults to `false`; the fsck driver opts in after
    /// reading the repo config.
    pub(crate) promisor_remote_present: bool,
    /// Graft points (`$GIT_DIR/shallow`), loaded lazily on the first
    /// [`ObjectReader::is_shallow_graft`] query. `$GIT_DIR` is taken to be
    /// the parent of `objects_dir`, matching the standard layout.
    pub(crate) shallow_grafts: Arc<std::sync::OnceLock<HashSet<ObjectId>>>,
    pub(crate) replacements: Arc<ObjectReplacements>,
}

fn read_shallow_grafts(shallow_file: &Path, format: ObjectFormat) -> HashSet<ObjectId> {
    let Ok(contents) = std::fs::read_to_string(shallow_file) else {
        return HashSet::new();
    };
    contents
        .lines()
        .filter_map(|line| ObjectId::from_hex(format, line.trim()).ok())
        .collect()
}

impl FileObjectDatabase {
    /// The object-id format (hash algorithm) this database was opened with.
    pub fn object_format(&self) -> ObjectFormat {
        self.format
    }

    /// The repository object directory this database reads from.
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    pub fn new(objects_dir: impl Into<PathBuf>, format: ObjectFormat) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: alternate_object_dirs(&objects_dir),
            objects_dir,
            format,
            pack_bytes: Arc::new(RwLock::new(HashMap::new())),
            pack_indexes: Arc::new(RwLock::new(HashMap::new())),
            pack_reverse_indexes: Arc::new(RwLock::new(HashMap::new())),
            multi_pack_indexes: Arc::new(RwLock::new(HashMap::new())),
            multi_pack_oid_lookups: Arc::new(RwLock::new(HashMap::new())),
            pack_registry: Arc::new(Mutex::new(None)),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
            pack_header_types: Arc::new(Mutex::new(HashMap::new())),
            promisor_objects: Arc::new(OnceLock::new()),
            promisor_remote_present: false,
            shallow_grafts: Arc::new(std::sync::OnceLock::new()),
            replacements: Arc::new(ObjectReplacements::default()),
        }
    }

    pub(crate) fn without_alternates(
        objects_dir: impl Into<PathBuf>,
        format: ObjectFormat,
    ) -> Self {
        let objects_dir = objects_dir.into();
        Self {
            loose: LooseObjectStore::new(objects_dir.clone(), format),
            alternates: Vec::new(),
            objects_dir,
            format,
            pack_bytes: Arc::new(RwLock::new(HashMap::new())),
            pack_indexes: Arc::new(RwLock::new(HashMap::new())),
            pack_reverse_indexes: Arc::new(RwLock::new(HashMap::new())),
            multi_pack_indexes: Arc::new(RwLock::new(HashMap::new())),
            multi_pack_oid_lookups: Arc::new(RwLock::new(HashMap::new())),
            pack_registry: Arc::new(Mutex::new(None)),
            decoded: Arc::new(Mutex::new(LruObjectCache::new(object_cache_budget()))),
            pack_deltas: Arc::new(Mutex::new(HashMap::new())),
            pack_header_types: Arc::new(Mutex::new(HashMap::new())),
            promisor_objects: Arc::new(OnceLock::new()),
            promisor_remote_present: false,
            shallow_grafts: Arc::new(std::sync::OnceLock::new()),
            replacements: Arc::new(ObjectReplacements::default()),
        }
    }

    pub fn from_git_dir(git_dir: impl AsRef<Path>, format: ObjectFormat) -> Self {
        let git_dir = git_dir.as_ref();
        let shared_repository = sley_formats::SharedRepositoryPermissions::from_git_dir(git_dir);
        let mut database = Self::new(repository_objects_dir(git_dir), format);
        database.loose = database.loose.with_shared_repository(shared_repository);
        database
    }

    /// Attach an explicit replacement map. Object enumeration/storage queries
    /// remain raw; object content and header reads dereference the map.
    pub fn with_replacements(mut self, replacements: ObjectReplacements) -> Self {
        self.replacements = Arc::new(replacements);
        self
    }

    pub fn replacements(&self) -> &ObjectReplacements {
        &self.replacements
    }

    pub fn replacement_oid(&self, oid: &ObjectId) -> Result<ObjectId> {
        self.replacements.resolve(oid)
    }

    /// Read object contents without consulting the injected replacement map.
    /// Used by raw enumeration surfaces such as `cat-file --batch-all-objects`.
    pub fn read_object_without_replacement(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        self.read_object_raw(oid)
    }

    /// Read an object header without consulting the injected replacement map.
    pub fn read_object_header_without_replacement(
        &self,
        oid: &ObjectId,
    ) -> Result<Option<(ObjectType, u64)>> {
        self.read_object_header_raw(oid)
    }

    /// Declare whether the owning repository has a promisor remote configured.
    /// Only when this holds does [`ObjectReader::is_promised_object`] treat
    /// objects in `.promisor` packs (and their transitive references) as
    /// promised — matching git's `is_promisor_object`, which is gated on
    /// `repo_has_promisor_remote()`. Callers that know the repo config (e.g. the
    /// fsck driver) opt in; readers built without config keep the safe default
    /// of `false`, so a stray `.promisor` sidecar never silently excuses a
    /// genuinely missing object.
    pub fn with_promisor_remote_present(mut self, present: bool) -> Self {
        self.promisor_remote_present = present;
        self
    }

    /// Drop cached pack registries, indexes, and decoded objects so the next read
    /// sees packs/objects installed after this handle was created (e.g. after
    /// `fetch` or `install_pack`). Long-lived `Repository` sessions call this
    /// via the owning repository's `refresh_objects` hook.
    pub fn refresh_read_cache(&self) {
        if let Ok(mut cache) = self.pack_registry.lock() {
            *cache = None;
        }
        self.pack_indexes.write().clear();
        self.multi_pack_indexes.write().clear();
        self.multi_pack_oid_lookups.write().clear();
        self.pack_bytes.write().clear();
        if let Ok(mut cache) = self.pack_deltas.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.pack_header_types.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.decoded.lock() {
            cache.clear();
        }
        self.loose.invalidate_cache();
    }

    pub fn loose(&self) -> &LooseObjectStore {
        &self.loose
    }

    pub fn presence_checker(&self) -> ObjectPresenceChecker {
        ObjectPresenceChecker::new(self.clone())
    }

    pub fn contains(&self, oid: &ObjectId) -> Result<bool> {
        if self.loose.exists(oid)? {
            return Ok(true);
        }
        if self.find_pack_containing(oid)?.is_some() {
            return Ok(true);
        }
        for alternate in &self.alternates {
            if Self::without_alternates(alternate, self.format).contains(oid)? {
                return Ok(true);
            }
        }
        // Reprepare-on-miss: a cached negative loose verdict may predate a
        // sibling write. Drop it and exact-probe once before reporting absence.
        self.loose.invalidate_cache();
        self.loose.exists(oid)
    }

    pub fn object_ids(&self) -> Result<Vec<ObjectId>> {
        let mut oids = object_ids_in_objects_dir(&self.objects_dir, self.format)?
            .into_iter()
            .collect::<HashSet<_>>();
        for alternate in &self.alternates {
            oids.extend(Self::without_alternates(alternate, self.format).object_ids()?);
        }
        let mut oids = oids.into_iter().collect::<Vec<_>>();
        oids.sort_by_key(ObjectId::to_hex);
        Ok(oids)
    }

    pub fn object_storage_info(&self, oid: &ObjectId) -> Result<Option<ObjectStorageInfo>> {
        if let Some(disk_size) = self.loose.disk_size(oid)? {
            return Ok(Some(ObjectStorageInfo {
                disk_size,
                deltabase: zero_oid(self.format)?,
            }));
        }
        if let Some(info) = self.packed_object_storage_info(oid)? {
            return Ok(Some(info));
        }
        for alternate in &self.alternates {
            if let Some(info) =
                Self::without_alternates(alternate, self.format).object_storage_info(oid)?
            {
                return Ok(Some(info));
            }
        }
        // Reprepare-on-miss: drop any stale negative loose cache and exact-probe
        // once before reporting absence (see `read_object`).
        self.loose.invalidate_cache();
        if let Some(disk_size) = self.loose.disk_size(oid)? {
            return Ok(Some(ObjectStorageInfo {
                disk_size,
                deltabase: zero_oid(self.format)?,
            }));
        }
        Ok(None)
    }

    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectPrefixResolution> {
        let mut matches = self.object_ids_with_prefix(prefix)?;
        Ok(match matches.len() {
            0 => ObjectPrefixResolution::Missing,
            1 => ObjectPrefixResolution::Unique(matches.remove(0)),
            _ => ObjectPrefixResolution::Ambiguous(matches),
        })
    }

    pub fn object_ids_with_prefix(&self, prefix: &str) -> Result<Vec<ObjectId>> {
        validate_object_id_prefix(self.format, prefix)?;
        let prefix_bytes = prefix.as_bytes();
        let mut matches =
            object_ids_with_prefix_in_objects_dir(&self.objects_dir, self.format, prefix_bytes)?
                .into_iter()
                .collect::<HashSet<_>>();
        for alternate in &self.alternates {
            for oid in
                Self::without_alternates(alternate, self.format).object_ids_with_prefix(prefix)?
            {
                matches.insert(oid);
            }
        }
        let mut matches = matches.into_iter().collect::<Vec<_>>();
        matches.sort_by_key(ObjectId::to_hex);
        Ok(matches)
    }

    /// The object type and content size of `oid` without decoding its full body —
    /// git's `cat-file --batch-check` fast path. Tries the decoded-object cache,
    /// then loose storage (inflating only the framing header), then packs (reading
    /// the entry header and, for deltas, only the delta's leading varints), then
    /// alternates. Returns `Ok(None)` if the object is not present.
    ///
    /// Unlike [`ObjectReader::read_object`], this never materializes the body, so it
    /// stays cheap on huge blobs and deep delta chains. It does not populate the
    /// decoded-object cache (nothing is decoded).
    pub fn read_object_header(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        let read_oid = self.replacements.resolve(oid)?;
        self.read_object_header_raw(&read_oid)
    }

    fn read_object_header_raw(&self, oid: &ObjectId) -> Result<Option<(ObjectType, u64)>> {
        if implied_empty_tree_object(self.format, oid).is_some() {
            return Ok(Some((ObjectType::Tree, 0)));
        }
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(Some((object.object_type, object.body.len() as u64)));
        }
        if let Some(header) = self.loose.read_header(oid)? {
            return Ok(Some(header));
        }
        if let Some(pack_lookup) = self.find_pack_containing(oid)? {
            let bytes = pack_lookup.pack_bytes(self)?;
            // Per-pack offset->type memo so the ofs-delta chain walk that resolves
            // a packed object's type runs at most once per chain across the batch,
            // instead of re-walking (and re-inflating each link's leading varints)
            // on every header read — the sley#26 super-linear cat-file --batch-check.
            let type_cache = pack_lookup.header_type_cache(self);
            let resolve_ref_base = |base: &ObjectId| {
                self.read_object_header_raw(base)
                    .map(|header| header.map(|(t, _)| t))
            };
            let header = match &type_cache {
                Some(cache) => {
                    let mut adapter = PackHeaderTypeCacheAdapter(cache);
                    sley_pack::read_object_header_at_with_cache(
                        &bytes,
                        pack_lookup.offset,
                        self.format,
                        resolve_ref_base,
                        &mut adapter,
                    )?
                }
                None => sley_pack::read_object_header_at(
                    &bytes,
                    pack_lookup.offset,
                    self.format,
                    resolve_ref_base,
                )?,
            };
            return Ok(Some(header));
        }
        for alternate in &self.alternates {
            if let Some(header) =
                Self::without_alternates(alternate, self.format).read_object_header_raw(oid)?
            {
                return Ok(Some(header));
            }
        }
        // Reprepare-on-miss: discard any stale negative loose cache and retry an
        // exact path probe once before reporting absence (see `read_object`).
        self.loose.invalidate_cache();
        if let Some(header) = self.loose.read_header(oid)? {
            return Ok(Some(header));
        }
        Ok(None)
    }

    pub(crate) fn read_packed_object(&self, oid: &ObjectId) -> Result<Option<Arc<EncodedObject>>> {
        // Memory-capped decoded-object cache first (delta-base reuse for ref-delta
        // bases that resolve back through the store + repeated whole-object reads).
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(Some(object));
        }
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        self.read_packed_object_at_lookup(oid, &pack_lookup)
            .map(Some)
    }

    pub(crate) fn read_packed_object_at_lookup(
        &self,
        oid: &ObjectId,
        pack_lookup: &PackLookup,
    ) -> Result<Arc<EncodedObject>> {
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(oid)
        {
            return Ok(object);
        }
        let bytes = pack_lookup.pack_bytes(self)?;
        // Per-pack delta-base cache (keyed by in-pack offset). Resolving an
        // ofs-delta chain reuses already-decoded bases instead of re-inflating the
        // whole chain on every read. Scoped to this pack's path so an offset key is
        // never applied to the wrong pack's bytes.
        let delta_cache = pack_lookup.delta_cache(self);
        let delta_adapter = delta_cache.as_ref().map(PackDeltaCacheAdapter);
        // Decode only this object at its offset (plus its delta-base chain). A
        // ref-delta base resolves through the full store (loose / other packs) and
        // reuses the decoded-object cache. No cache lock is held across the decode,
        // so the recursive resolver re-entry (which may re-enter read_object) is
        // safe.
        let resolve_ref_base = |base: &ObjectId| self.read_object_raw(base).map(Some);
        let resolve_ofs_base =
            |base_offset| self.read_ofs_delta_base_from_other_sources(pack_lookup, base_offset);
        let object = match &delta_adapter {
            Some(adapter) => sley_pack::read_object_at_with_cache_and_ofs_base_arc(
                &bytes,
                pack_lookup.offset,
                self.format,
                resolve_ref_base,
                resolve_ofs_base,
                adapter,
            )?,
            None => sley_pack::read_object_at_with_ofs_base_arc(
                &bytes,
                pack_lookup.offset,
                self.format,
                resolve_ref_base,
                resolve_ofs_base,
            )?,
        };
        // Trust the index → offset mapping rather than re-hashing every decoded
        // object on read (see `verify_reads_enabled`); this re-hash dominated
        // bulk-read cost. Opt back in with `SLEY_VERIFY_READS` for a paranoid check.
        if verify_reads_enabled() {
            let actual = object.object_id(self.format)?;
            if actual != *oid {
                return Err(GitError::InvalidObject(format!(
                    "pack object id mismatch: index says {oid}, decoded {actual}"
                )));
            }
        }
        if let Ok(mut cache) = self.decoded.lock() {
            cache.put(*oid, Arc::clone(&object));
        }
        Ok(object)
    }

    /// The per-pack delta-base cache for `pack_path`, creating it on first use.
    /// Returns `None` only if the shared map's lock is poisoned, in which case the
    /// caller falls back to an uncached decode (correctness preserved).
    pub(crate) fn pack_delta_cache(&self, pack_path: &Path) -> Option<Arc<Mutex<LruOffsetCache>>> {
        let mut caches = self.pack_deltas.lock().ok()?;
        let cache = caches.entry(pack_path.to_path_buf()).or_insert_with(|| {
            Arc::new(Mutex::new(LruOffsetCache::new(delta_base_cache_budget())))
        });
        Some(Arc::clone(cache))
    }

    /// The per-pack header-type memo for `pack_path`, creating it on first use.
    /// Returns `None` only if the shared map's lock is poisoned, in which case the
    /// caller falls back to an unmemoized header walk (correctness preserved).
    pub(crate) fn pack_header_type_cache(&self, pack_path: &Path) -> Option<PackHeaderTypeCache> {
        let mut caches = self.pack_header_types.lock().ok()?;
        let cache = caches
            .entry(pack_path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())));
        Some(Arc::clone(cache))
    }

    /// Backing bytes of the pack at `pack_path`, loaded at most once per database
    /// handle (cached, shared across clones). Memory-mapped under the `mmap` feature,
    /// otherwise read into the heap. On a poisoned lock it falls back to loading
    /// without caching, preserving correctness.
    pub(crate) fn cached_pack_bytes(&self, pack_path: &Path) -> Result<Arc<PackData>> {
        if let Some(bytes) = self.pack_bytes.read().get(pack_path) {
            return Ok(Arc::clone(bytes));
        }
        let bytes = Arc::new(load_pack_data(pack_path)?);
        self.pack_bytes
            .write()
            .insert(pack_path.to_path_buf(), Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Parsed index for the `.idx` at `index_path`, parsed at most once per
    /// database handle. On a poisoned lock it falls back to parsing without
    /// caching, preserving correctness.
    pub(crate) fn cached_pack_index(&self, index_path: &Path) -> Result<Arc<PackIndexViewData>> {
        if let Some(index) = self.pack_indexes.read().get(index_path) {
            return Ok(Arc::clone(index));
        }
        let index_bytes = load_pack_index_data(index_path)?;
        let index = Arc::new(PackIndexViewData::parse_trusted_source_without_checksum(
            index_bytes,
            self.format,
        )?);
        self.pack_indexes
            .write()
            .insert(index_path.to_path_buf(), Arc::clone(&index));
        Ok(index)
    }

    /// Optional reverse index for the `.idx` at `index_path`, loaded at most once
    /// per database handle when a matching `.rev` sidecar is present.
    pub(crate) fn cached_pack_reverse_index(
        &self,
        index_path: &Path,
        index: &PackIndexViewData,
    ) -> Result<Option<Arc<PackReverseIndex>>> {
        if let Some(cached) = self.pack_reverse_indexes.read().get(index_path) {
            return Ok(cached.as_ref().map(Arc::clone));
        }
        let rev_path = index_path.with_extension("rev");
        let reverse = if rev_path.exists() {
            let bytes = fs::read(&rev_path)?;
            match PackReverseIndex::parse(&bytes, self.format, index.count) {
                Ok(parsed) if parsed.pack_checksum == index.pack_checksum => Some(Arc::new(parsed)),
                _ => None,
            }
        } else {
            None
        };
        self.pack_reverse_indexes
            .write()
            .insert(index_path.to_path_buf(), reverse.as_ref().map(Arc::clone));
        Ok(reverse)
    }

    pub(crate) fn cached_multi_pack_index_oid_lookup(
        &self,
        midx_path: &Path,
    ) -> Result<Option<Arc<MultiPackIndexOidLookup>>> {
        if !midx_path.exists() {
            return Ok(None);
        }
        if let Some(midx) = self.multi_pack_oid_lookups.read().get(midx_path) {
            return Ok(Some(Arc::clone(midx)));
        }
        let bytes = load_multi_pack_index_lookup_data(midx_path)?;
        let midx = match MultiPackIndexOidLookup::parse(bytes, self.format) {
            Ok(midx) => Arc::new(midx),
            Err(GitError::InvalidFormat(message))
                if message.starts_with("multi-pack-index hash id ") =>
            {
                let actual = message
                    .strip_prefix("multi-pack-index hash id ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("0");
                let expected = match self.format {
                    ObjectFormat::Sha1 => 1,
                    ObjectFormat::Sha256 => 2,
                };
                eprintln!(
                    "error: multi-pack-index hash version {actual} does not match version {expected}"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        self.multi_pack_oid_lookups
            .write()
            .insert(midx_path.to_path_buf(), Arc::clone(&midx));
        Ok(Some(midx))
    }

    pub(crate) fn cached_multi_pack_index(
        &self,
        midx_path: &Path,
    ) -> Result<Option<Arc<MultiPackIndex>>> {
        if !midx_path.exists() {
            return Ok(None);
        }
        if let Some(midx) = self.multi_pack_indexes.read().get(midx_path) {
            return Ok(Some(Arc::clone(midx)));
        }
        let bytes = load_multi_pack_index_lookup_data(midx_path)?;
        let midx = match MultiPackIndex::parse(bytes.as_bytes(), self.format) {
            Ok(midx) => Arc::new(midx),
            Err(GitError::InvalidFormat(message))
                if message.starts_with("multi-pack-index hash id ") =>
            {
                let actual = message
                    .strip_prefix("multi-pack-index hash id ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("0");
                let expected = match self.format {
                    ObjectFormat::Sha1 => 1,
                    ObjectFormat::Sha256 => 2,
                };
                eprintln!(
                    "error: multi-pack-index hash version {actual} does not match version {expected}"
                );
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        self.multi_pack_indexes
            .write()
            .insert(midx_path.to_path_buf(), Arc::clone(&midx));
        Ok(Some(midx))
    }

    /// Registry snapshot for this database's pack directory. With `force_rescan`,
    /// the directory is re-read; when the fingerprint and pack set match the
    /// cached snapshot, the same `Arc` is returned so miss handling can tell that
    /// no new packs appeared.
    pub(crate) fn cached_pack_registry(
        &self,
        pack_dir: &Path,
        force_rescan: bool,
    ) -> Result<Arc<PackRegistrySnapshot>> {
        if !force_rescan && let Some(registry) = self.cached_loaded_pack_registry(pack_dir)? {
            return Ok(registry);
        }
        let scanned = Arc::new(scan_pack_registry(pack_dir, self.format)?);
        if let Ok(mut cache) = self.pack_registry.lock() {
            match cache.as_ref() {
                Some(existing)
                    if existing.fingerprint == scanned.fingerprint
                        && same_registered_pack_set(&existing.packs, &scanned.packs) =>
                {
                    return Ok(Arc::clone(existing));
                }
                _ => {
                    *cache = Some(Arc::clone(&scanned));
                }
            }
        }
        Ok(scanned)
    }

    pub(crate) fn find_in_pack_registry(
        &self,
        registry: Arc<PackRegistrySnapshot>,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let hinted_pack_index = registry.cached_hint();
        if let Some(pack_index) = hinted_pack_index {
            let pack = &registry.packs[pack_index];
            match pack.index(self.format) {
                Ok(index) => {
                    if let Some(entry) = index.find(oid) {
                        return Ok(Some(PackLookup::from_registered(
                            Arc::clone(pack),
                            entry.offset,
                        )));
                    }
                }
                Err(_) => {
                    eprintln!("error: packfile {} index unavailable", pack.pack.display());
                }
            }
        }
        for (pack_index, pack) in registry.packs.iter().enumerate() {
            if Some(pack_index) == hinted_pack_index {
                continue;
            }
            let index = match pack.index(self.format) {
                Ok(index) => index,
                Err(_) => {
                    eprintln!("error: packfile {} index unavailable", pack.pack.display());
                    continue;
                }
            };
            if let Some(entry) = index.find(oid) {
                registry.remember_hint(pack_index);
                return Ok(Some(PackLookup::from_registered(
                    Arc::clone(pack),
                    entry.offset,
                )));
            }
        }
        Ok(None)
    }

    /// Read `oid` from any pack *other than* the one named by `exclude`, used as
    /// a corruption fallback: a redundant packed copy survives one pack's
    /// damage. Scans the on-disk `.idx` files directly (bypassing the registry
    /// cache, whose first hit is the excluded pack) and decodes from the first
    /// other pack that both indexes the object and parses cleanly.
    pub(crate) fn read_packed_object_from_other_packs(
        &self,
        oid: &ObjectId,
        exclude: &PackLookup,
    ) -> Result<Option<Arc<EncodedObject>>> {
        let pack_dir = self.objects_dir.join("pack");
        let Ok(entries) = fs::read_dir(&pack_dir) else {
            return Ok(None);
        };
        let excluded_pack = exclude.pack_path().to_path_buf();
        for entry in entries {
            let idx_path = entry?.path();
            if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
                continue;
            }
            let pack_path = idx_path.with_extension("pack");
            if pack_path == excluded_pack {
                continue;
            }
            let Ok(idx_bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&idx_bytes, self.format) else {
                continue;
            };
            let Some(entry) = index.find(oid) else {
                continue;
            };
            let candidate = PackLookup::from_path(pack_path, entry.offset);
            if let Ok(object) = self.read_packed_object_at_lookup(oid, &candidate) {
                return Ok(Some(object));
            }
        }
        Ok(None)
    }

    pub(crate) fn pack_oid_at_offset(
        &self,
        pack_lookup: &PackLookup,
        offset: u64,
    ) -> Result<Option<ObjectId>> {
        let index_path = match &pack_lookup.registered {
            Some(pack) => pack.idx.clone(),
            None => pack_lookup.pack.with_extension("idx"),
        };
        match pack_lookup.pack_index(self) {
            Ok(index) => {
                if let Some(reverse) = self.cached_pack_reverse_index(&index_path, &index)? {
                    Ok(reverse.oid_at_offset(&index, offset))
                } else {
                    Ok(index.oid_at_offset_linear(offset))
                }
            }
            Err(_) => self.midx_oid_for_pack_offset(pack_lookup, offset),
        }
    }

    pub(crate) fn read_ofs_delta_base_from_other_sources(
        &self,
        pack_lookup: &PackLookup,
        base_offset: u64,
    ) -> Result<Option<Arc<EncodedObject>>> {
        let Some(base_oid) = self.pack_oid_at_offset(pack_lookup, base_offset)? else {
            return Ok(None);
        };
        if let Ok(mut cache) = self.decoded.lock()
            && let Some(object) = cache.get(&base_oid)
        {
            return Ok(Some(object));
        }
        if let Ok(object) = self.loose.read_object(&base_oid) {
            return Ok(Some(object));
        }
        if let Some(object) = self.read_packed_object_from_other_packs(&base_oid, pack_lookup)? {
            return Ok(Some(object));
        }
        for alternate in &self.alternates {
            if let Ok(object) =
                Self::without_alternates(alternate, self.format).read_object(&base_oid)
            {
                return Ok(Some(object));
            }
        }
        Ok(None)
    }

    pub(crate) fn find_pack_containing(&self, oid: &ObjectId) -> Result<Option<PackLookup>> {
        if oid.format() != self.format {
            return Err(GitError::InvalidObjectId(format!(
                "object {oid} uses {}, store uses {}",
                oid.format().name(),
                self.format.name()
            )));
        }
        let pack_dir = self.objects_dir.join("pack");
        // Hot path: a previously cached pack registry or multi-pack-index already
        // names every pack, and locating `oid` in them is pure in-memory index
        // work. Try that first so a warm handle does not parse indexes or hash
        // pack paths on every lookup.
        if let Some(midx) = self.cached_loaded_multi_pack_index_oid_lookup()
            && let Some(pack_paths) = self.midx_oid_lookup_pack_paths(&pack_dir, &midx, oid)?
        {
            return Ok(Some(pack_paths));
        }
        if let Some(registry) = self.cached_loaded_pack_registry(&pack_dir)?
            && let Some(pack_paths) = self.find_in_pack_registry(registry, oid)?
        {
            return Ok(Some(pack_paths));
        }

        if !pack_dir.exists() {
            return Ok(None);
        }
        if let Some(pack_paths) = self.find_midx_pack_containing(&pack_dir, oid)? {
            return Ok(Some(pack_paths));
        }
        // Search the cached registry first. On a complete miss, re-scan the
        // directory once (picking up any pack added since the registry was
        // cached) and search again, so newly written packs are still found.
        let registry = self.cached_pack_registry(&pack_dir, false)?;
        if let Some(pack_paths) = self.find_in_pack_registry(Arc::clone(&registry), oid)? {
            return Ok(Some(pack_paths));
        }
        let refreshed = self.cached_pack_registry(&pack_dir, true)?;
        if Arc::ptr_eq(&registry, &refreshed) {
            // The re-scan produced the same registry, so nothing new appeared.
            return Ok(None);
        }
        self.find_in_pack_registry(refreshed, oid)
    }

    pub(crate) fn packed_object_storage_info(
        &self,
        oid: &ObjectId,
    ) -> Result<Option<ObjectStorageInfo>> {
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let index = pack_lookup.pack_index(self).ok();
        let pack = match pack_lookup.pack_bytes(self) {
            Ok(pack) => Some(pack),
            Err(_err) if index.is_some() => None,
            Err(err) => return Err(err),
        };
        let trailer_offset = pack
            .as_ref()
            .map(|pack| {
                (pack.len() as u64)
                    .checked_sub(self.format.raw_len() as u64)
                    .ok_or_else(|| {
                        GitError::InvalidFormat("pack file shorter than checksum".into())
                    })
            })
            .transpose()?;
        let delta_base = match &pack {
            Some(pack) => pack_entry_delta_base(self.format, pack, pack_lookup.offset)?,
            None => None,
        };
        let delta_base_offset = match &delta_base {
            Some(PackDeltaBase::Offset(offset)) => Some(*offset),
            Some(PackDeltaBase::Ref(_)) | None => None,
        };
        let offset_info = if let Some(index) = &index {
            scan_pack_index_offsets(index, pack_lookup.offset, trailer_offset, delta_base_offset)?
        } else if let Some(pack) = &pack {
            let end_offset =
                scan_pack_offsets_without_index(self.format, pack, pack_lookup.offset)?
                    .ok_or_else(|| {
                        GitError::InvalidFormat(format!(
                            "pack offset {} not found",
                            pack_lookup.offset
                        ))
                    })?;
            let delta_base_oid = match delta_base_offset {
                Some(offset) => self
                    .midx_oid_for_pack_offset(&pack_lookup, offset)?
                    .ok_or_else(|| {
                        GitError::InvalidFormat(format!("ofs-delta base offset {offset} not found"))
                    })?,
                None => zero_oid(self.format)?,
            };
            PackIndexOffsetInfo {
                end_offset,
                delta_base_oid: delta_base_offset.map(|_| delta_base_oid),
            }
        } else {
            return Err(GitError::InvalidFormat(
                "packed object metadata source unavailable".into(),
            ));
        };
        let disk_size = offset_info
            .end_offset
            .checked_sub(pack_lookup.offset)
            .ok_or_else(|| GitError::InvalidFormat("pack index offsets are not sorted".into()))?;
        let deltabase = match delta_base {
            Some(PackDeltaBase::Offset(_)) => offset_info.delta_base_oid.ok_or_else(|| {
                // scan_pack_index_offsets returns Err when delta_base_offset is
                // Some but no matching entry is found, so this is unreachable for
                // valid packs; propagate as an error rather than panic to keep a
                // malformed pack from taking down the process if that invariant
                // ever drifts.
                GitError::InvalidFormat("ofs-delta base oid missing from pack index".into())
            })?,
            Some(PackDeltaBase::Ref(oid)) => oid,
            None => zero_oid(self.format)?,
        };
        Ok(Some(ObjectStorageInfo {
            disk_size,
            deltabase,
        }))
    }

    pub(crate) fn midx_oid_for_pack_offset(
        &self,
        pack_lookup: &PackLookup,
        offset: u64,
    ) -> Result<Option<ObjectId>> {
        let pack_dir = self.objects_dir.join("pack");
        let midx_path = pack_dir.join("multi-pack-index");
        let Some(midx) = self.cached_multi_pack_index(&midx_path)? else {
            return Ok(None);
        };
        let Some(pack_name) = pack_lookup
            .pack_path()
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return Ok(None);
        };
        let idx_name = pack_name
            .strip_suffix(".pack")
            .map(|stem| format!("{stem}.idx"))
            .unwrap_or_else(|| pack_name.to_string());
        let Some(pack_int_id) = midx
            .pack_names
            .iter()
            .position(|candidate| candidate == &idx_name)
        else {
            return Ok(None);
        };
        Ok(midx
            .objects
            .iter()
            .find(|entry| entry.pack_int_id == pack_int_id as u32 && entry.offset == offset)
            .map(|entry| entry.oid))
    }

    pub(crate) fn find_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let midx_path = pack_dir.join("multi-pack-index");
        if let Some(midx) = self.cached_multi_pack_index_oid_lookup(&midx_path)?
            && let Some(pack_lookup) = self.midx_oid_lookup_pack_paths(pack_dir, &midx, oid)?
        {
            return Ok(Some(pack_lookup));
        }
        self.find_incremental_midx_pack_containing(pack_dir, oid)
    }

    pub(crate) fn midx_oid_lookup_pack_paths(
        &self,
        pack_dir: &Path,
        midx: &MultiPackIndexOidLookup,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let Some(entry) = midx.find(oid)? else {
            return Ok(None);
        };
        let Some(pack_name) = midx.pack_name(entry.pack_int_id) else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        };
        let pack_file_name = pack_name
            .strip_suffix(".idx")
            .map(|stem| format!("{stem}.pack"))
            .unwrap_or_else(|| pack_name.to_string());
        let pack = pack_dir.join(pack_file_name);
        Ok(Some(PackLookup::from_path(pack, entry.offset)))
    }

    pub(crate) fn find_incremental_midx_pack_containing(
        &self,
        pack_dir: &Path,
        oid: &ObjectId,
    ) -> Result<Option<PackLookup>> {
        let chain = read_incremental_midx_chain(pack_dir)?;
        if chain.is_empty() {
            return Ok(None);
        }
        let midx_dir = pack_dir.join("multi-pack-index.d");
        for checksum in chain.iter().rev() {
            let path = midx_dir.join(format!("multi-pack-index-{checksum}.midx"));
            if !path.exists() {
                continue;
            }
            let bytes = load_multi_pack_index_lookup_data(&path)?;
            let midx = match MultiPackIndexOidLookup::parse(bytes, self.format) {
                Ok(midx) => midx,
                Err(_) => continue,
            };
            if let Some(pack_lookup) = self.midx_oid_lookup_pack_paths(pack_dir, &midx, oid)? {
                return Ok(Some(pack_lookup));
            }
        }
        Ok(None)
    }

    pub(crate) fn cached_loaded_multi_pack_index_oid_lookup(
        &self,
    ) -> Option<Arc<MultiPackIndexOidLookup>> {
        let midx_path = self.objects_dir.join("pack").join("multi-pack-index");
        self.multi_pack_oid_lookups
            .read()
            .get(&midx_path)
            .map(Arc::clone)
    }

    /// The pack registry for `pack_dir` *only if already scanned and cached* —
    /// never touches the filesystem. Used by the lookup hot path to skip
    /// per-object pack-dir metadata checks once a handle is warm. A cold cache
    /// returns `None`, so the caller falls back to the scanning path. A complete
    /// miss still forces one rescan, preserving the new-pack discovery semantics.
    pub(crate) fn cached_loaded_pack_registry(
        &self,
        _pack_dir: &Path,
    ) -> Result<Option<Arc<PackRegistrySnapshot>>> {
        let cache = match self.pack_registry.lock() {
            Ok(cache) => cache,
            Err(_) => return Ok(None),
        };
        Ok(cache.as_ref().map(Arc::clone))
    }
}
impl ObjectReader for FileObjectDatabase {
    fn reusable_delta_base(&self, oid: &ObjectId) -> Result<Option<ObjectId>> {
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(None);
        };
        let pack = pack_lookup.pack_bytes(self)?;
        match pack_entry_delta_base(self.format, &pack, pack_lookup.offset)? {
            Some(PackDeltaBase::Ref(base_oid)) => Ok(Some(base_oid)),
            Some(PackDeltaBase::Offset(base_offset)) => {
                let base_oid = self
                    .pack_oid_at_offset(&pack_lookup, base_offset)?
                    .ok_or_else(|| {
                        GitError::InvalidFormat(format!(
                            "ofs-delta base offset {base_offset} not found"
                        ))
                    })?;
                Ok(Some(base_oid))
            }
            None => Ok(None),
        }
    }

    fn is_promised_object(&self, oid: &ObjectId) -> bool {
        // Gate on a configured promisor remote, exactly like git's
        // `is_promisor_object` (which short-circuits when
        // `repo_has_promisor_remote()` is false). Without this, a `.promisor`
        // sidecar left in an ordinary repository would wrongly excuse missing
        // objects from fsck connectivity checks.
        self.promisor_remote_present && self.promisor_objects().contains(oid)
    }

    fn has_shallow_grafts(&self) -> bool {
        !self
            .shallow_grafts
            .get_or_init(|| {
                let shallow_file = self
                    .objects_dir
                    .parent()
                    .map(|git_dir| git_dir.join("shallow"));
                match shallow_file {
                    Some(path) => read_shallow_grafts(&path, self.format),
                    None => HashSet::new(),
                }
            })
            .is_empty()
    }

    fn is_shallow_graft(&self, oid: &ObjectId) -> bool {
        self.shallow_grafts
            .get_or_init(|| {
                let shallow_file = self
                    .objects_dir
                    .parent()
                    .map(|git_dir| git_dir.join("shallow"));
                match shallow_file {
                    Some(path) => read_shallow_grafts(&path, self.format),
                    None => HashSet::new(),
                }
            })
            .contains(oid)
    }

    fn read_object(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        let read_oid = self.replacements.resolve(oid)?;
        self.read_object_raw(&read_oid)
    }
}

impl FileObjectDatabase {
    fn read_object_raw(&self, oid: &ObjectId) -> Result<Arc<EncodedObject>> {
        if let Some(object) = implied_empty_tree_object(self.format, oid) {
            return Ok(object);
        }
        // A corrupt loose copy must not shadow a good packed copy: git's
        // `oid_object_info_extended` consults every source, so a repacked object
        // whose loose file was later corrupted still reads fine from the pack. If
        // a packed copy exists, prefer it WITHOUT touching the corrupt loose file
        // (which would otherwise emit a spurious `inflate:` diagnostic on each
        // probe). Only when no pack copy exists do we read (and, if corrupt,
        // surface the error from) the loose file.
        if let Some(pack_lookup) = self.find_pack_containing(oid)? {
            match self.read_packed_object_at_lookup(oid, &pack_lookup) {
                Ok(object) => return Ok(object),
                Err(GitError::NotFound(_)) => {}
                // A corrupt packed copy must not be fatal when another good copy
                // exists: git's `oid_object_info_extended` keeps consulting the
                // remaining sources (loose, other packs, alternates) when a pack
                // read fails. Fall through to the loose/other-pack probes and
                // only surface the packed error if every source comes up empty.
                Err(packed_err) => {
                    if let Ok(object) = self.loose.read_object(oid) {
                        return Ok(object);
                    }
                    // Try any *other* pack that also holds the object (a
                    // redundant copy survives one pack's corruption).
                    if let Some(object) =
                        self.read_packed_object_from_other_packs(oid, &pack_lookup)?
                    {
                        return Ok(object);
                    }
                    for alternate in &self.alternates {
                        if let Ok(object) =
                            Self::without_alternates(alternate, self.format).read_object_raw(oid)
                        {
                            return Ok(object);
                        }
                    }
                    return Err(packed_err);
                }
            }
        }
        let loose_err = match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::NotFound(_)) => None,
            Err(err) => Some(err),
        };
        if let Some(object) = self.read_packed_object(oid)? {
            return Ok(object);
        }
        for alternate in &self.alternates {
            match Self::without_alternates(alternate, self.format).read_object_raw(oid) {
                Ok(object) => return Ok(object),
                Err(GitError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        // Hard miss against every store. If an earlier enumeration built a loose
        // cache, an object written loose afterward by a sibling handle could have
        // been skipped above. Mirror git's `oid_object_info_extended`
        // reprepare-on-miss: drop stale cache state and retry an exact loose path
        // probe once before declaring the object missing.
        self.loose.invalidate_cache();
        match self.loose.read_object(oid) {
            Ok(object) => return Ok(object),
            Err(GitError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        // No good copy in any store. If the local loose copy was corrupt (not
        // merely absent), surface that error — it is more specific than a plain
        // "not found".
        if let Some(err) = loose_err {
            return Err(err);
        }
        Err(GitError::object_not_found_in(
            *oid,
            MissingObjectContext::Read,
        ))
    }
}
impl FileObjectDatabase {
    fn promisor_objects(&self) -> &HashSet<ObjectId> {
        self.promisor_objects.get_or_init(|| {
            let mut promised =
                promisor_pack_object_ids(&self.objects_dir, self.format).unwrap_or_default();
            let mut pending = promised.iter().copied().collect::<Vec<_>>();
            while let Some(oid) = pending.pop() {
                let Ok(object) = self.read_object(&oid) else {
                    continue;
                };
                for link in promisor_object_links(self.format, &object) {
                    if promised.insert(link) {
                        pending.push(link);
                    }
                }
            }
            promised
        })
    }

    fn freshen_existing_object(&self, oid: &ObjectId) -> Result<bool> {
        if self.freshen_loose_object(oid)? {
            return Ok(true);
        }
        if self.freshen_packed_object(oid)? {
            return Ok(true);
        }
        for alternate in &self.alternates {
            if Self::without_alternates(alternate, self.format).freshen_existing_object(oid)? {
                return Ok(true);
            }
        }
        // A previous negative loose-cache probe may predate a sibling write.
        self.loose.invalidate_cache();
        self.freshen_loose_object(oid)
    }

    fn freshen_loose_object(&self, oid: &ObjectId) -> Result<bool> {
        let path = self.loose.object_path(oid)?;
        freshen_file_mtime(&path)
    }

    fn freshen_packed_object(&self, oid: &ObjectId) -> Result<bool> {
        let Some(pack_lookup) = self.find_pack_containing(oid)? else {
            return Ok(false);
        };
        if !pack_lookup.pack_path().with_extension("mtimes").exists() {
            return freshen_file_mtime(pack_lookup.pack_path());
        }

        // Cruft packs carry per-object mtimes in their `.mtimes` sidecar. Git
        // deliberately never freshens the pack itself: doing so would make
        // every unreachable object in it look recent. If another, non-cruft
        // pack also contains the object, freshen that copy; otherwise return a
        // miss so the caller writes a new loose copy with its own mtime.
        let pack_dir = self.objects_dir.join("pack");
        let Ok(entries) = fs::read_dir(pack_dir) else {
            return Ok(false);
        };
        for entry in entries {
            let idx_path = entry?.path();
            if idx_path.extension().and_then(|ext| ext.to_str()) != Some("idx")
                || idx_path.with_extension("mtimes").exists()
            {
                continue;
            }
            let pack_path = idx_path.with_extension("pack");
            if !pack_path.exists() {
                continue;
            }
            let Ok(index_bytes) = fs::read(&idx_path) else {
                continue;
            };
            let Ok(index) = PackIndex::parse(&index_bytes, self.format) else {
                continue;
            };
            if index.find(oid).is_some() {
                return freshen_file_mtime(&pack_path);
            }
        }
        Ok(false)
    }
}
pub(crate) fn freshen_file_mtime(path: &Path) -> Result<bool> {
    let file = match fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(GitError::Io(err.to_string())),
    };
    file.set_modified(std::time::SystemTime::now())
        .map_err(|err| GitError::Io(err.to_string()))?;
    Ok(true)
}

pub(crate) fn promisor_pack_object_ids(
    objects_dir: &Path,
    format: ObjectFormat,
) -> Result<HashSet<ObjectId>> {
    let pack_dir = objects_dir.join("pack");
    let mut oids = HashSet::new();
    if !pack_dir.exists() {
        return Ok(oids);
    }
    for entry in fs::read_dir(pack_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        if !path.with_extension("pack").exists() || !path.with_extension("promisor").exists() {
            continue;
        }
        let index = PackIndex::parse(&fs::read(path)?, format)?;
        oids.extend(index.entries.into_iter().map(|entry| entry.oid));
    }
    Ok(oids)
}

pub(crate) fn promisor_object_links(format: ObjectFormat, object: &EncodedObject) -> Vec<ObjectId> {
    match object.object_type {
        ObjectType::Commit => Commit::parse_ref(format, &object.body)
            .map(|commit| {
                let mut links = Vec::with_capacity(commit.parents.len() + 1);
                links.push(commit.tree);
                links.extend(commit.parents);
                links
            })
            .unwrap_or_default(),
        ObjectType::Tree => TreeEntries::new(format, &object.body)
            .filter_map(|entry| entry.ok().map(|entry| entry.oid))
            .collect(),
        ObjectType::Tag => Tag::parse_ref(format, &object.body)
            .map(|tag| vec![tag.object])
            .unwrap_or_default(),
        ObjectType::Blob => Vec::new(),
    }
}
impl ObjectWriter for FileObjectDatabase {
    fn write_object(&self, object: EncodedObject) -> Result<ObjectId> {
        // Mirror git's freshen semantics (`write_object_file`:
        // `freshen_packed_object || freshen_loose_object`): an object already
        // present in a normal pack is not written again, but its backing pack is
        // touched so concurrent GC treats it as recent. Cruft packs are excluded
        // from freshening, so rewriting a cruft-only object creates a loose copy
        // whose per-object mtime can be honored by the next cruft repack.
        let oid = object.object_id(self.format)?;
        if self.freshen_existing_object(&oid)? {
            return Ok(oid);
        }
        self.loose.write_object(object)
    }
}
