//! Bounded, positional decoding of individual pack entries.
//!
//! This module deliberately reuses the crate's entry-header, OFS-offset, and
//! delta-application parsers. It only supplies the random-access I/O and the
//! iterative chain planner around that authoritative grammar.

use super::*;
use flate2::{Decompress, FlushDecompress, Status};
use std::collections::{HashMap, HashSet};
use std::io;

const ENTRY_PREFIX_BYTES: usize = 64;
const INFLATE_CHUNK_BYTES: usize = 8 * 1024;

/// A source that can read pack bytes positionally without changing shared
/// cursor state or requiring the complete pack to be resident in memory.
pub trait PackReadSource {
    /// Total source length, including the pack trailer.
    fn len(&self) -> io::Result<u64>;

    /// Read bytes beginning at `offset`, with the same short-read semantics as
    /// [`std::io::Read::read`]. Returning `0` means end of source.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;

    /// Whether this source contains no bytes.
    fn is_empty(&self) -> io::Result<bool> {
        self.len().map(|len| len == 0)
    }
}

/// A borrowed in-memory source, useful for parity tests and already-bounded
/// pack buffers. The decoder borrows the slice for its entire lifetime.
#[derive(Debug, Clone, Copy)]
pub struct SlicePackSource<'a> {
    bytes: &'a [u8],
}

impl<'a> SlicePackSource<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}

impl PackReadSource for SlicePackSource<'_> {
    fn len(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(remaining) = self.bytes.get(start..) else {
            return Ok(0);
        };
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        Ok(count)
    }
}

impl PackReadSource for Vec<u8> {
    fn len(&self) -> io::Result<u64> {
        Ok(self.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(remaining) = self.get(start..) else {
            return Ok(0);
        };
        let count = remaining.len().min(buf.len());
        buf[..count].copy_from_slice(&remaining[..count]);
        Ok(count)
    }
}

#[cfg(unix)]
impl PackReadSource for std::fs::File {
    fn len(&self) -> io::Result<u64> {
        self.metadata().map(|metadata| metadata.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
}

#[cfg(windows)]
impl PackReadSource for std::fs::File {
    fn len(&self) -> io::Result<u64> {
        self.metadata().map(|metadata| metadata.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }
}

/// Hard limits applied by every targeted read through a
/// [`BoundedPackDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackReadLimits {
    /// Maximum number of delta entries between a target and its base.
    pub max_delta_depth: usize,
    /// Maximum logical decoded object and delta bytes owned or actively used by
    /// the decoder at one time. The bound is checked before allocation. Allocator
    /// slack/capacity, collection metadata, and fixed-size I/O scratch buffers
    /// are not included, so this is deliberately not an RSS or heap-usage bound.
    /// A resolved immutable REF base is counted once while active even when its
    /// `Arc` is also retained by its outcome or lookup table; the decoder reuses
    /// that allocation rather than copying it. Returned objects cease to count
    /// after the call unless cached.
    pub max_materialized_bytes: usize,
    /// Maximum logical decoded body bytes retained between calls. The effective
    /// cache ceiling is also capped by `max_materialized_bytes`.
    pub max_cached_bytes: usize,
}

impl Default for PackReadLimits {
    fn default() -> Self {
        Self {
            max_delta_depth: DEFAULT_PACK_DEPTH,
            max_materialized_bytes: 64 * 1024 * 1024,
            max_cached_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Which explicit decoder limit rejected a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackLimitKind {
    DeltaDepth,
    MaterializedBytes,
}

/// Deterministic details for a rejected limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimitError {
    pub kind: PackLimitKind,
    pub limit: usize,
    pub attempted: usize,
}

/// A body allocation failed after every configured decoder limit had passed.
///
/// This is deliberately distinct from [`PackReadError::Limit`]: allocator
/// availability is an environmental resource failure, not a deterministic
/// rejection by [`PackReadLimits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackAllocationError {
    /// Logical body bytes requested from the allocator.
    pub requested: usize,
    /// Other active logical body/delta bytes at the allocation point.
    pub active: usize,
    /// Logical body bytes retained in the decoder cache at the allocation point.
    pub cached: usize,
}

/// Error returned by bounded targeted decoding.
#[derive(Debug)]
pub enum PackReadError {
    Limit(PackLimitError),
    Allocation(PackAllocationError),
    Source(io::Error),
    Pack(GitError),
}

impl fmt::Display for PackReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(error) => write!(
                formatter,
                "pack {:?} limit exceeded: limit {}, attempted {}",
                error.kind, error.limit, error.attempted
            ),
            Self::Allocation(error) => write!(
                formatter,
                "pack body allocation failed: requested {}, active {}, cached {}",
                error.requested, error.active, error.cached
            ),
            Self::Source(error) => write!(formatter, "pack source read failed: {error}"),
            Self::Pack(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PackReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Pack(error) => Some(error),
            Self::Limit(_) | Self::Allocation(_) => None,
        }
    }
}

impl From<io::Error> for PackReadError {
    fn from(error: io::Error) -> Self {
        Self::Source(error)
    }
}

impl From<GitError> for PackReadError {
    fn from(error: GitError) -> Self {
        Self::Pack(error)
    }
}

/// Opaque identifier for one source registered with a [`BoundedPackDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackSourceId(usize);

/// A stable entry location within a registered pack source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackObjectLocation {
    source: PackSourceId,
    offset: u64,
}

impl PackObjectLocation {
    pub const fn new(source: PackSourceId, offset: u64) -> Self {
        Self { source, offset }
    }

    pub const fn source(self) -> PackSourceId {
        self.source
    }

    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// Immutable, identity-bound materialization accepted as a non-pack REF base.
///
/// The object ID and structural depth are compiler-controlled and travel with
/// the same [`Arc`] as the body. Pack-derived values can only be obtained from
/// [`PackReadOutcome::resolved_base`]. Loose/non-delta objects can only be
/// introduced through [`RefDeltaBases::insert_loose`], which computes their ID
/// and does not expose a reusable depth token.
///
/// ```compile_fail
/// # use sley_core::ObjectId;
/// # use sley_object::EncodedObject;
/// # use sley_pack::ResolvedPackObject;
/// # use std::sync::Arc;
/// # fn forged(object: Arc<EncodedObject>, oid: ObjectId) {
/// let _ = ResolvedPackObject { object, oid, depth: 0, origin: None };
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedPackObject {
    object: Arc<EncodedObject>,
    oid: ObjectId,
    depth: usize,
    origin: Option<PackObjectLocation>,
}

impl ResolvedPackObject {
    pub fn object(&self) -> &EncodedObject {
        &self.object
    }

    pub const fn object_id(&self) -> ObjectId {
        self.oid
    }

    pub const fn delta_depth(&self) -> usize {
        self.depth
    }
}

/// Precomputed REF-base lookup used during one or more targeted reads.
///
/// Pack locations are followed by the decoder on its explicit heap work list;
/// lookup cannot recursively invoke another decoder while an outer decoder is
/// on the call stack. Immutable loose/materialized bases are reused by `Arc`
/// and counted in the same per-read materialization budget.
#[derive(Debug, Clone, Default)]
pub struct RefDeltaBases {
    entries: HashMap<ObjectId, RefDeltaBase>,
}

#[derive(Debug, Clone)]
enum RefDeltaBase {
    Location(PackObjectLocation),
    Resolved(ResolvedPackObject),
}

impl RefDeltaBases {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_location(&mut self, oid: ObjectId, location: PackObjectLocation) {
        self.entries.insert(oid, RefDeltaBase::Location(location));
    }

    /// Register a base supplied by a loose/non-delta object store. Its identity
    /// is derived from the immutable object; callers cannot provide a separate
    /// object ID or depth value.
    pub fn insert_loose(
        &mut self,
        object: Arc<EncodedObject>,
        format: ObjectFormat,
    ) -> Result<ObjectId> {
        self.insert_loose_with_cancel(object, format, CancelFlag::never())
    }

    pub fn insert_loose_with_cancel(
        &mut self,
        object: Arc<EncodedObject>,
        format: ObjectFormat,
        cancel: CancelFlag<'_>,
    ) -> Result<ObjectId> {
        let oid = cancellable_object_id(&object, format, cancel)?;
        cancel.check()?;
        let resolved = RefDeltaBase::Resolved(ResolvedPackObject {
            object,
            oid,
            depth: 0,
            origin: None,
        });
        self.replace_transactionally(oid, resolved, || cancel.check())?;
        Ok(oid)
    }

    fn replace_transactionally<F>(
        &mut self,
        oid: ObjectId,
        replacement: RefDeltaBase,
        post_insert: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let previous = self.entries.insert(oid, replacement);
        if let Err(error) = post_insert() {
            match previous {
                Some(previous) => {
                    self.entries.insert(oid, previous);
                }
                None => {
                    self.entries.remove(&oid);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn insert_resolved(&mut self, resolved: ResolvedPackObject) -> ObjectId {
        let oid = resolved.oid;
        self.entries.insert(oid, RefDeltaBase::Resolved(resolved));
        oid
    }

    fn get(&self, oid: &ObjectId) -> Option<&RefDeltaBase> {
        self.entries.get(oid)
    }
}

/// Usage measured for one targeted read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackReadStats {
    /// Total bytes returned by [`PackReadSource`] during this call, including
    /// entry prefixes and compressed read chunks. Bytes are counted once at the
    /// decoder's single positional-read chokepoint.
    source_bytes_read: u64,
    /// Exact zlib input bytes consumed while decoding entries. Unlike
    /// `source_bytes_read`, this excludes entry prefixes and input read-ahead
    /// left after `StreamEnd`.
    compressed_bytes_read: u64,
    /// Highest simultaneous logical decoded body/delta byte total during this
    /// call, including decoder cache contents.
    peak_materialized_bytes: usize,
    /// Logical decoded body bytes retained after this call.
    cached_bytes: usize,
    cached_objects: usize,
    cache_evictions: u64,
    /// Number of deltas resolved for the requested object.
    delta_depth: usize,
}

impl PackReadStats {
    fn start(cached_bytes: usize) -> Self {
        Self {
            source_bytes_read: 0,
            compressed_bytes_read: 0,
            peak_materialized_bytes: cached_bytes,
            cached_bytes: 0,
            cached_objects: 0,
            cache_evictions: 0,
            delta_depth: 0,
        }
    }

    pub const fn source_bytes_read(&self) -> u64 {
        self.source_bytes_read
    }

    pub const fn compressed_bytes_read(&self) -> u64 {
        self.compressed_bytes_read
    }

    pub const fn peak_materialized_bytes(&self) -> usize {
        self.peak_materialized_bytes
    }

    pub const fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }

    pub const fn cached_objects(&self) -> usize {
        self.cached_objects
    }

    pub const fn cache_evictions(&self) -> u64 {
        self.cache_evictions
    }

    pub const fn delta_depth(&self) -> usize {
        self.delta_depth
    }
}

/// One decoded object and the resources measured while producing it.
///
/// Outcome construction and depth statistics are intentionally private. This
/// prevents safe callers from forging lower structural depth and converting it
/// into authoritative REF-base evidence.
///
/// ```compile_fail
/// # use sley_pack::{PackReadOutcome, PackReadStats};
/// # use std::sync::Arc;
/// # fn forged(object: Arc<sley_object::EncodedObject>) {
/// let stats = PackReadStats {
///     source_bytes_read: 0,
///     compressed_bytes_read: 0,
///     peak_materialized_bytes: 0,
///     cached_bytes: 0,
///     cached_objects: 0,
///     cache_evictions: 0,
///     delta_depth: 0,
/// };
/// let _ = PackReadOutcome { object, stats };
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct PackReadOutcome {
    object: Arc<EncodedObject>,
    stats: PackReadStats,
    oid: ObjectId,
    depth: usize,
    origin: PackObjectLocation,
}

impl PackReadOutcome {
    pub fn object(&self) -> &EncodedObject {
        &self.object
    }

    pub const fn stats(&self) -> &PackReadStats {
        &self.stats
    }

    pub const fn object_id(&self) -> ObjectId {
        self.oid
    }

    pub fn resolved_base(&self) -> ResolvedPackObject {
        ResolvedPackObject {
            object: Arc::clone(&self.object),
            oid: self.oid,
            depth: self.depth,
            origin: Some(self.origin),
        }
    }
}

#[derive(Debug)]
struct CachedObject {
    object: Arc<EncodedObject>,
    oid: ObjectId,
    bytes: usize,
    depth: usize,
    less_recent: Option<PackObjectLocation>,
    more_recent: Option<PackObjectLocation>,
}

#[derive(Debug)]
struct ByteCache {
    budget: usize,
    used: usize,
    entries: HashMap<PackObjectLocation, CachedObject>,
    least_recent: Option<PackObjectLocation>,
    most_recent: Option<PackObjectLocation>,
    evictions: u64,
}

impl ByteCache {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            entries: HashMap::new(),
            least_recent: None,
            most_recent: None,
            evictions: 0,
        }
    }

    fn get(
        &mut self,
        location: PackObjectLocation,
        cancel: CancelFlag<'_>,
    ) -> Result<Option<(Arc<EncodedObject>, ObjectId, usize)>> {
        cancel.check()?;
        let Some(entry) = self.entries.get(&location) else {
            cancel.check()?;
            return Ok(None);
        };
        let found = (Arc::clone(&entry.object), entry.oid, entry.depth);
        self.touch(location);
        cancel.check()?;
        Ok(Some(found))
    }

    fn peek(&self, location: PackObjectLocation) -> Option<(Arc<EncodedObject>, ObjectId, usize)> {
        let entry = self.entries.get(&location)?;
        Some((Arc::clone(&entry.object), entry.oid, entry.depth))
    }

    fn contains_same(&self, location: PackObjectLocation, object: &Arc<EncodedObject>) -> bool {
        self.entries
            .get(&location)
            .is_some_and(|cached| Arc::ptr_eq(&cached.object, object))
    }

    fn insert(
        &mut self,
        location: PackObjectLocation,
        object: Arc<EncodedObject>,
        oid: ObjectId,
        depth: usize,
        cancel: CancelFlag<'_>,
    ) -> Result<()> {
        cancel.check()?;
        let bytes = object.body.len();
        // Zero-length bodies would otherwise permit unbounded cache metadata
        // under a byte-only budget without retaining any useful body storage.
        if bytes == 0 || bytes > self.budget || self.budget == 0 {
            cancel.check()?;
            return Ok(());
        }
        let previous_bytes = self
            .entries
            .get(&location)
            .map_or(0, |previous| previous.bytes);
        while self
            .used
            .checked_sub(previous_bytes)
            .and_then(|retained| retained.checked_add(bytes))
            .is_none_or(|projected| projected > self.budget)
        {
            cancel.check()?;
            if !self.evict_one_except(Some(location), cancel)? {
                return Ok(());
            }
        }
        cancel.check()?;
        if self.entries.contains_key(&location) {
            self.remove_entry(location);
        }
        self.used += bytes;
        self.entries.insert(
            location,
            CachedObject {
                object,
                oid,
                bytes,
                depth,
                less_recent: None,
                more_recent: None,
            },
        );
        self.link_as_most_recent(location);
        Ok(())
    }

    fn evict_one_except(
        &mut self,
        pinned: Option<PackObjectLocation>,
        cancel: CancelFlag<'_>,
    ) -> Result<bool> {
        self.evict_one_except_with(pinned, || cancel.check())
    }

    fn evict_one_except_with<F>(
        &mut self,
        pinned: Option<PackObjectLocation>,
        mut poll: F,
    ) -> Result<bool>
    where
        F: FnMut() -> Result<()>,
    {
        poll()?;
        let location = match self.least_recent {
            Some(location) if Some(location) != pinned => Some(location),
            Some(location) => self
                .entries
                .get(&location)
                .and_then(|cached| cached.more_recent),
            None => None,
        };
        let Some(location) = location else {
            poll()?;
            return Ok(false);
        };
        self.remove_entry(location);
        self.evictions += 1;
        poll()?;
        Ok(true)
    }

    fn clear(&mut self, cancel: CancelFlag<'_>) -> Result<()> {
        self.clear_with(|| cancel.check())
    }

    fn clear_with<F>(&mut self, mut poll: F) -> Result<()>
    where
        F: FnMut() -> Result<()>,
    {
        poll()?;
        let mut removed = 0usize;
        while let Some(location) = self.least_recent {
            self.remove_entry(location);
            removed += 1;
            if removed.is_multiple_of(64) {
                poll()?;
            }
        }
        poll()?;
        Ok(())
    }

    fn touch(&mut self, location: PackObjectLocation) {
        if self.most_recent == Some(location) {
            return;
        }
        self.unlink(location);
        self.link_as_most_recent(location);
    }

    fn remove_entry(&mut self, location: PackObjectLocation) -> Option<CachedObject> {
        self.unlink(location);
        let cached = self.entries.remove(&location)?;
        debug_assert!(self.used >= cached.bytes);
        self.used -= cached.bytes;
        Some(cached)
    }

    fn unlink(&mut self, location: PackObjectLocation) {
        let Some((less_recent, more_recent)) = self
            .entries
            .get(&location)
            .map(|cached| (cached.less_recent, cached.more_recent))
        else {
            return;
        };
        if let Some(less_recent) = less_recent {
            if let Some(cached) = self.entries.get_mut(&less_recent) {
                cached.more_recent = more_recent;
            }
        } else {
            self.least_recent = more_recent;
        }
        if let Some(more_recent) = more_recent {
            if let Some(cached) = self.entries.get_mut(&more_recent) {
                cached.less_recent = less_recent;
            }
        } else {
            self.most_recent = less_recent;
        }
        if let Some(cached) = self.entries.get_mut(&location) {
            cached.less_recent = None;
            cached.more_recent = None;
        }
    }

    fn link_as_most_recent(&mut self, location: PackObjectLocation) {
        let previous = self.most_recent;
        if let Some(cached) = self.entries.get_mut(&location) {
            cached.less_recent = previous;
            cached.more_recent = None;
        } else {
            return;
        }
        if let Some(previous) = previous {
            if let Some(cached) = self.entries.get_mut(&previous) {
                cached.more_recent = Some(location);
            }
        } else {
            self.least_recent = Some(location);
        }
        self.most_recent = Some(location);
    }
}

#[derive(Debug)]
struct EntryPlan {
    location: PackObjectLocation,
    header: EntryHeader,
    data_offset: u64,
    base: Option<DeltaBase>,
}

struct PackSourceState<S> {
    source: S,
    format: ObjectFormat,
    trailer_offset: u64,
}

/// A targeted decoder tied to one or more registered pack sources.
///
/// Delta chains are planned in a heap vector and resolved from base to target,
/// so call-stack use is constant with respect to attacker-controlled depth.
/// The internal cache and materialization budget are shared across every
/// registered source. Each source's length and contents, including open
/// [`std::fs::File`] values, must remain stable for the decoder's lifetime.
/// Cache accounting is by logical body bytes, not entry count, and
/// [`Self::clear_cache`] releases every decoder-held object. Cross-pack REF
/// chains must be registered as locations before the read; the decoder follows
/// them iteratively without a resolver callback or nested decoder call.
pub struct BoundedPackDecoder<S> {
    sources: Vec<PackSourceState<S>>,
    limits: PackReadLimits,
    cache: ByteCache,
}

impl<S: PackReadSource> BoundedPackDecoder<S> {
    pub fn new(
        source: S,
        format: ObjectFormat,
        limits: PackReadLimits,
    ) -> std::result::Result<Self, PackReadError> {
        let source = Self::open_source(source, format)?;
        Ok(Self {
            sources: vec![source],
            limits,
            cache: ByteCache::new(limits.max_cached_bytes.min(limits.max_materialized_bytes)),
        })
    }

    fn open_source(
        source: S,
        format: ObjectFormat,
    ) -> std::result::Result<PackSourceState<S>, PackReadError> {
        let source_len = source.len()?;
        let trailer_len = format.raw_len() as u64;
        let trailer_offset = source_len
            .checked_sub(trailer_len)
            .ok_or_else(|| GitError::InvalidFormat("pack smaller than its trailer".into()))?;
        Ok(PackSourceState {
            source,
            format,
            trailer_offset,
        })
    }

    pub const fn primary_source(&self) -> PackSourceId {
        PackSourceId(0)
    }

    pub fn add_source(
        &mut self,
        source: S,
        format: ObjectFormat,
    ) -> std::result::Result<PackSourceId, PackReadError> {
        let source = Self::open_source(source, format)?;
        let id = PackSourceId(self.sources.len());
        self.sources.push(source);
        Ok(id)
    }

    pub const fn limits(&self) -> PackReadLimits {
        self.limits
    }

    pub fn cached_bytes(&self) -> usize {
        self.cache.used
    }

    pub fn cached_objects(&self) -> usize {
        self.cache.entries.len()
    }

    /// Drop decoded objects retained between targeted reads, polling between
    /// bounded batches. Cancellation may leave a valid partially-cleared cache;
    /// its byte accounting and LRU order remain exact for the retained entries.
    pub fn clear_cache(
        &mut self,
        cancel: CancelFlag<'_>,
    ) -> std::result::Result<(), PackReadError> {
        self.cache.clear(cancel)?;
        Ok(())
    }

    /// Decode an entry in the primary source without loading a complete pack.
    pub fn read_object_at(
        &mut self,
        offset: u64,
        ref_bases: &RefDeltaBases,
    ) -> std::result::Result<PackReadOutcome, PackReadError> {
        self.read_object_at_with_cancel(offset, ref_bases, CancelFlag::never())
    }

    pub fn read_object_at_with_cancel(
        &mut self,
        offset: u64,
        ref_bases: &RefDeltaBases,
        cancel: CancelFlag<'_>,
    ) -> std::result::Result<PackReadOutcome, PackReadError> {
        self.read_object_at_location_with_cancel(
            PackObjectLocation::new(self.primary_source(), offset),
            ref_bases,
            cancel,
        )
    }

    /// Decode an entry in any registered source. REF locations in `ref_bases`
    /// are followed on the same explicit work list as OFS links, keeping stack
    /// use constant across cold multi-pack chains.
    pub fn read_object_at_location(
        &mut self,
        location: PackObjectLocation,
        ref_bases: &RefDeltaBases,
    ) -> std::result::Result<PackReadOutcome, PackReadError> {
        self.read_object_at_location_with_cancel(location, ref_bases, CancelFlag::never())
    }

    /// Cancel-aware form of [`Self::read_object_at_location`]. The flag is
    /// polled during chain planning, positional reads, inflate, object-ID
    /// hashing, every delta command, and between cache maintenance operations.
    pub fn read_object_at_location_with_cancel(
        &mut self,
        location: PackObjectLocation,
        ref_bases: &RefDeltaBases,
        cancel: CancelFlag<'_>,
    ) -> std::result::Result<PackReadOutcome, PackReadError> {
        cancel.check()?;
        let target_format = self.source_state(location)?.format;
        let evictions_before = self.cache.evictions;
        let mut stats = PackReadStats::start(self.cache.used);
        if let Some((object, oid, depth)) = self.cache.get(location, cancel)? {
            stats.delta_depth = depth;
            self.finish_stats(&mut stats, evictions_before);
            return Ok(PackReadOutcome {
                object,
                stats,
                oid,
                depth,
                origin: location,
            });
        }

        let mut visited = HashSet::new();
        let mut deltas = Vec::new();
        let mut current_location = location;
        let mut base_object: Option<(
            Arc<EncodedObject>,
            ObjectId,
            Option<PackObjectLocation>,
            usize,
        )> = None;
        let mut base_entry = None;

        loop {
            cancel.check()?;
            self.source_state(current_location)?;
            if !visited.insert(current_location) {
                return Err(GitError::InvalidFormat("pack delta cycle detected".into()).into());
            }
            if current_location != location
                && let Some((object, oid, cached_depth)) = self.cache.peek(current_location)
            {
                let full_depth = deltas.len().saturating_add(cached_depth);
                self.enforce_depth(full_depth)?;
                base_object = Some((object, oid, Some(current_location), cached_depth));
                break;
            }
            let entry = self.read_entry_plan(current_location, cancel, &mut stats)?;
            cancel.check()?;
            match entry.base.clone() {
                None => {
                    base_entry = Some(entry);
                    break;
                }
                Some(base) => {
                    let depth = deltas.len().saturating_add(1);
                    self.enforce_depth(depth)?;
                    deltas.push(entry);
                    match base {
                        DeltaBase::Offset(base_offset) => {
                            current_location =
                                PackObjectLocation::new(current_location.source, base_offset);
                        }
                        DeltaBase::Ref(base_oid) => {
                            cancel.check()?;
                            match ref_bases.get(&base_oid) {
                                Some(RefDeltaBase::Location(base_location)) => {
                                    current_location = *base_location;
                                }
                                Some(RefDeltaBase::Resolved(resolved)) => {
                                    if resolved.oid != base_oid {
                                        return Err(GitError::InvalidObject(format!(
                                            "resolved REF base identity mismatch: expected {base_oid}, got {}",
                                            resolved.oid
                                        ))
                                        .into());
                                    }
                                    let full_depth = deltas.len().saturating_add(resolved.depth);
                                    self.enforce_depth(full_depth)?;
                                    let pinned = resolved.origin.filter(|origin| {
                                        self.cache.contains_same(*origin, &resolved.object)
                                    });
                                    let active = if pinned.is_some() {
                                        0
                                    } else {
                                        resolved.object.body.len()
                                    };
                                    self.ensure_materialized(
                                        0, active, pinned, cancel, &mut stats,
                                    )?;
                                    base_object = Some((
                                        Arc::clone(&resolved.object),
                                        resolved.oid,
                                        pinned,
                                        resolved.depth,
                                    ));
                                    break;
                                }
                                None => {
                                    return Err(GitError::not_found(format!(
                                        "ref-delta base object {base_oid}"
                                    ))
                                    .into());
                                }
                            }
                        }
                    }
                }
            }
        }

        let cached_base_depth = base_object.as_ref().map_or(0, |(_, _, _, depth)| *depth);
        stats.delta_depth = deltas.len().saturating_add(cached_base_depth);
        self.enforce_depth(stats.delta_depth)?;
        let (mut object, mut object_oid, mut pinned_cache) = match (base_object, base_entry) {
            (Some((object, oid, pinned, _)), None) => (object, Some(oid), pinned),
            (None, Some(entry)) => {
                let object_type = object_type_for_entry(entry.header.kind)?;
                let body = self.inflate_entry(&entry, 0, None, cancel, &mut stats)?;
                let object = Arc::new(EncodedObject::new(object_type, body));
                (object, None, None)
            }
            _ => {
                return Err(
                    GitError::InvalidFormat("pack delta base planning failed".into()).into(),
                );
            }
        };

        for delta_entry in deltas.iter().rev() {
            cancel.check()?;
            if let Some(DeltaBase::Ref(expected_oid)) = delta_entry.base.as_ref() {
                let actual_oid = match object_oid {
                    Some(oid) if oid.format() == expected_oid.format() => oid,
                    _ => cancellable_object_id(&object, expected_oid.format(), cancel)?,
                };
                if actual_oid != *expected_oid {
                    return Err(GitError::InvalidObject(format!(
                        "resolved REF base identity mismatch: expected {expected_oid}, got {actual_oid}"
                    ))
                    .into());
                }
            }
            let base_bytes = if pinned_cache.is_some() {
                0
            } else {
                object.body.len()
            };
            let delta =
                self.inflate_entry(delta_entry, base_bytes, pinned_cache, cancel, &mut stats)?;
            let plan = plan_pack_delta(&object.body, &delta)?;
            let result_size_u64 = plan.result_size;
            let result_size = usize::try_from(result_size_u64).map_err(|_| {
                PackReadError::Limit(PackLimitError {
                    kind: PackLimitKind::MaterializedBytes,
                    limit: self.limits.max_materialized_bytes,
                    attempted: usize::MAX,
                })
            })?;
            let mut resolved = self.allocate_body(
                result_size,
                base_bytes.saturating_add(delta.len()),
                pinned_cache,
                cancel,
                &mut stats,
            )?;
            apply_pack_delta_exact(&object.body, &delta, plan, &mut resolved, cancel)?;
            object = Arc::new(EncodedObject::new(object.object_type, resolved));
            object_oid = None;
            pinned_cache = None;
        }

        cancel.check()?;
        let oid = cancellable_object_id(&object, target_format, cancel)?;
        self.cache.insert(
            location,
            Arc::clone(&object),
            oid,
            stats.delta_depth,
            cancel,
        )?;
        self.finish_stats(&mut stats, evictions_before);
        let depth = stats.delta_depth;
        Ok(PackReadOutcome {
            object,
            stats,
            oid,
            depth,
            origin: location,
        })
    }

    fn finish_stats(&self, stats: &mut PackReadStats, evictions_before: u64) {
        stats.cached_bytes = self.cache.used;
        stats.cached_objects = self.cache.entries.len();
        stats.cache_evictions = self.cache.evictions.saturating_sub(evictions_before);
        stats.peak_materialized_bytes = stats.peak_materialized_bytes.max(self.cache.used);
    }

    fn enforce_depth(&self, depth: usize) -> std::result::Result<(), PackReadError> {
        if depth > self.limits.max_delta_depth {
            return Err(PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::DeltaDepth,
                limit: self.limits.max_delta_depth,
                attempted: depth,
            }));
        }
        Ok(())
    }

    fn read_entry_plan(
        &self,
        location: PackObjectLocation,
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<EntryPlan, PackReadError> {
        let source = self.source_state(location)?;
        let offset = location.offset;
        if offset >= source.trailer_offset {
            return Err(GitError::InvalidFormat("pack object offset out of range".into()).into());
        }
        let available =
            usize::try_from((source.trailer_offset - offset).min(ENTRY_PREFIX_BYTES as u64))
                .unwrap_or(ENTRY_PREFIX_BYTES);
        let mut prefix = [0u8; ENTRY_PREFIX_BYTES];
        self.read_exact_at(
            location.source,
            offset,
            &mut prefix[..available],
            cancel,
            stats,
        )?;
        let bytes = &prefix[..available];
        let mut cursor = 0usize;
        let header = parse_entry_header(bytes, &mut cursor)?;
        let base = match header.kind {
            PackObjectKind::OfsDelta => Some(DeltaBase::Offset(parse_ofs_delta_base_offset(
                bytes,
                &mut cursor,
                offset,
            )?)),
            PackObjectKind::RefDelta => {
                let raw_len = source.format.raw_len();
                let end = cursor.checked_add(raw_len).ok_or_else(|| {
                    GitError::InvalidFormat("ref-delta base offset overflow".into())
                })?;
                let raw = bytes.get(cursor..end).ok_or_else(|| {
                    GitError::InvalidFormat("truncated ref-delta base object id".into())
                })?;
                cursor = end;
                Some(DeltaBase::Ref(ObjectId::from_raw(source.format, raw)?))
            }
            _ => None,
        };
        let data_offset = offset
            .checked_add(cursor as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack object offset overflow".into()))?;
        Ok(EntryPlan {
            location,
            header,
            data_offset,
            base,
        })
    }

    fn inflate_entry(
        &mut self,
        entry: &EntryPlan,
        active_bytes: usize,
        pinned_cache: Option<PackObjectLocation>,
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<Vec<u8>, PackReadError> {
        cancel.check()?;
        let expected = usize::try_from(entry.header.size).map_err(|_| {
            PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.limits.max_materialized_bytes,
                attempted: usize::MAX,
            })
        })?;
        let source = self.source_state(entry.location)?;
        let source_id = entry.location.source;
        let trailer_offset = source.trailer_offset;
        let mut body = self.allocate_body(expected, active_bytes, pinned_cache, cancel, stats)?;

        let mut decompressor = Decompress::new(true);
        let mut input = [0u8; INFLATE_CHUNK_BYTES];
        let mut output = [0u8; INFLATE_CHUNK_BYTES];
        let mut input_start = 0usize;
        let mut input_end = 0usize;
        let mut source_offset = entry.data_offset;

        loop {
            cancel.check()?;
            if input_start == input_end {
                if source_offset >= trailer_offset {
                    return Err(GitError::InvalidObject("truncated zlib stream".into()).into());
                }
                let wanted = usize::try_from(
                    (trailer_offset - source_offset).min(INFLATE_CHUNK_BYTES as u64),
                )
                .unwrap_or(INFLATE_CHUNK_BYTES);
                let read = self.read_source_at(
                    source_id,
                    source_offset,
                    &mut input[..wanted],
                    cancel,
                    stats,
                )?;
                if read == 0 {
                    return Err(GitError::InvalidObject("truncated zlib stream".into()).into());
                }
                source_offset = source_offset
                    .checked_add(read as u64)
                    .ok_or_else(|| GitError::InvalidFormat("pack source offset overflow".into()))?;
                input_start = 0;
                input_end = read;
            }

            let before_in = decompressor.total_in();
            let before_out = decompressor.total_out();
            let status = decompressor
                .decompress(
                    &input[input_start..input_end],
                    &mut output,
                    FlushDecompress::None,
                )
                .map_err(|error| {
                    GitError::InvalidObject(format!("zlib inflate failed: {error}"))
                })?;
            let consumed =
                usize::try_from(decompressor.total_in() - before_in).unwrap_or(usize::MAX);
            let produced =
                usize::try_from(decompressor.total_out() - before_out).unwrap_or(usize::MAX);
            input_start = input_start.saturating_add(consumed);
            stats.compressed_bytes_read =
                stats.compressed_bytes_read.saturating_add(consumed as u64);
            let attempted = body.len().saturating_add(produced);
            if attempted > expected {
                return Err(GitError::InvalidObject(format!(
                    "pack object declared {} bytes, decoded more than {}",
                    entry.header.size, expected
                ))
                .into());
            }
            body.extend_from_slice(&output[..produced]);

            if status == Status::StreamEnd {
                if body.len() != expected {
                    return Err(GitError::InvalidObject(format!(
                        "pack object declared {} bytes, decoded {}",
                        entry.header.size,
                        body.len()
                    ))
                    .into());
                }
                return Ok(body);
            }
            if consumed == 0 && produced == 0 && input_start < input_end {
                return Err(GitError::InvalidObject("zlib inflate made no progress".into()).into());
            }
        }
    }

    fn ensure_materialized(
        &mut self,
        active_bytes: usize,
        additional_bytes: usize,
        pinned_cache: Option<PackObjectLocation>,
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<(), PackReadError> {
        cancel.check()?;
        let working = active_bytes.saturating_add(additional_bytes);
        if working > self.limits.max_materialized_bytes {
            return Err(PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.limits.max_materialized_bytes,
                attempted: working,
            }));
        }
        while self.cache.used.saturating_add(working) > self.limits.max_materialized_bytes {
            cancel.check()?;
            if !self.cache.evict_one_except(pinned_cache, cancel)? {
                break;
            }
        }
        let total = self.cache.used.saturating_add(working);
        if total > self.limits.max_materialized_bytes {
            return Err(PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.limits.max_materialized_bytes,
                attempted: total,
            }));
        }
        stats.peak_materialized_bytes = stats.peak_materialized_bytes.max(total);
        Ok(())
    }

    fn allocate_body(
        &mut self,
        requested: usize,
        active_bytes: usize,
        pinned_cache: Option<PackObjectLocation>,
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<Vec<u8>, PackReadError> {
        cancel.check()?;
        self.ensure_materialized(active_bytes, requested, pinned_cache, cancel, stats)?;
        cancel.check()?;
        let mut body = Vec::new();
        let allocation = body.try_reserve_exact(requested);
        cancel.check()?;
        allocation.map_err(|_| {
            PackReadError::Allocation(PackAllocationError {
                requested,
                active: active_bytes,
                cached: self.cache.used,
            })
        })?;
        Ok(body)
    }

    fn read_source_at(
        &self,
        source_id: PackSourceId,
        offset: u64,
        buf: &mut [u8],
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<usize, PackReadError> {
        cancel.check()?;
        let source = self
            .sources
            .get(source_id.0)
            .ok_or_else(|| GitError::InvalidFormat("unknown pack source id".into()))?;
        let read_result = source.source.read_at(offset, buf);
        cancel.check()?;
        let read = read_result?;
        if read > buf.len() {
            return Err(GitError::InvalidFormat(
                "pack source returned more bytes than requested".into(),
            )
            .into());
        }
        stats.source_bytes_read = stats.source_bytes_read.saturating_add(read as u64);
        Ok(read)
    }

    fn read_exact_at(
        &self,
        source_id: PackSourceId,
        mut offset: u64,
        mut buf: &mut [u8],
        cancel: CancelFlag<'_>,
        stats: &mut PackReadStats,
    ) -> std::result::Result<(), PackReadError> {
        while !buf.is_empty() {
            let read = self.read_source_at(source_id, offset, buf, cancel, stats)?;
            if read == 0 {
                return Err(GitError::InvalidFormat("truncated pack entry header".into()).into());
            }
            offset = offset
                .checked_add(read as u64)
                .ok_or_else(|| GitError::InvalidFormat("pack source offset overflow".into()))?;
            buf = &mut buf[read..];
        }
        Ok(())
    }

    fn source_state(
        &self,
        location: PackObjectLocation,
    ) -> std::result::Result<&PackSourceState<S>, PackReadError> {
        self.sources
            .get(location.source.0)
            .ok_or_else(|| GitError::InvalidFormat("unknown pack source id".into()).into())
    }
}

fn cancellable_object_id(
    object: &EncodedObject,
    format: ObjectFormat,
    cancel: CancelFlag<'_>,
) -> Result<ObjectId> {
    cancel.check()?;
    let mut digest = StreamingDigest::new(format);
    digest.update(object.object_type.as_str().as_bytes());
    digest.update(b" ");
    let body_len = object.body.len().to_string();
    digest.update(body_len.as_bytes());
    digest.update(b"\0");
    for chunk in object.body.chunks(INFLATE_CHUNK_BYTES) {
        cancel.check()?;
        digest.update(chunk);
    }
    cancel.check()?;
    digest.finalize()
}

fn object_type_for_entry(kind: PackObjectKind) -> Result<ObjectType> {
    match kind {
        PackObjectKind::Commit => Ok(ObjectType::Commit),
        PackObjectKind::Tree => Ok(ObjectType::Tree),
        PackObjectKind::Blob => Ok(ObjectType::Blob),
        PackObjectKind::Tag => Ok(ObjectType::Tag),
        PackObjectKind::OfsDelta | PackObjectKind::RefDelta => Err(GitError::InvalidFormat(
            "delta pack entry decoded without a base".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_oid() -> ObjectId {
        ObjectId::from_raw(ObjectFormat::Sha1, &[0; 20]).expect("object id")
    }

    fn location(offset: u64) -> PackObjectLocation {
        PackObjectLocation::new(PackSourceId(0), offset)
    }

    fn insert_cached(cache: &mut ByteCache, offset: u64, bytes: usize) {
        cache
            .insert(
                location(offset),
                Arc::new(EncodedObject::new(
                    ObjectType::Blob,
                    vec![offset as u8; bytes],
                )),
                test_oid(),
                0,
                CancelFlag::never(),
            )
            .expect("cache insert");
    }

    #[test]
    fn cache_eviction_is_constant_work_and_preserves_lru_accounting() {
        let mut cache = ByteCache::new(512);
        for offset in 0..256 {
            insert_cached(&mut cache, offset, 1);
        }
        cache
            .get(location(0), CancelFlag::never())
            .expect("cache get")
            .expect("cached entry");
        assert_eq!(cache.least_recent, Some(location(1)));
        assert_eq!(cache.most_recent, Some(location(0)));

        let mut polls = 0;
        assert!(
            cache
                .evict_one_except_with(None, || {
                    polls += 1;
                    Ok(())
                })
                .expect("eviction")
        );
        assert_eq!(polls, 2, "eviction work must not grow with cache size");
        assert!(!cache.entries.contains_key(&location(1)));
        assert_eq!(cache.least_recent, Some(location(2)));
        assert_eq!(cache.most_recent, Some(location(0)));
        assert_eq!(cache.entries.len(), 255);
        assert_eq!(cache.used, 255);
        assert_eq!(cache.evictions, 1);
    }

    #[test]
    fn cache_replacement_and_pinned_eviction_keep_exact_order_and_bytes() {
        let mut cache = ByteCache::new(8);
        insert_cached(&mut cache, 1, 1);
        insert_cached(&mut cache, 2, 2);
        insert_cached(&mut cache, 3, 3);
        assert_eq!(cache.used, 6);

        // Replacing the least-recent entry pins it during capacity eviction.
        insert_cached(&mut cache, 1, 5);
        assert_eq!(cache.used, 8);
        assert!(!cache.entries.contains_key(&location(2)));
        assert!(cache.entries.contains_key(&location(3)));
        assert!(cache.entries.contains_key(&location(1)));
        assert_eq!(cache.least_recent, Some(location(3)));
        assert_eq!(cache.most_recent, Some(location(1)));
        assert_eq!(cache.evictions, 1);
    }

    #[test]
    fn cancelled_clear_leaves_exact_partially_cleared_lru_state() {
        let mut cache = ByteCache::new(256);
        for offset in 0..256 {
            insert_cached(&mut cache, offset, 1);
        }
        let mut polls = 0;
        let error = cache
            .clear_with(|| {
                polls += 1;
                if polls == 3 {
                    return Err(GitError::Cancelled);
                }
                Ok(())
            })
            .expect_err("clear must observe cancellation between batches");
        assert!(matches!(error, GitError::Cancelled));
        assert_eq!(cache.entries.len(), 128);
        assert_eq!(cache.used, 128);
        assert_eq!(cache.least_recent, Some(location(128)));
        assert_eq!(cache.most_recent, Some(location(255)));
        assert_eq!(
            cache.entries[&location(128)].less_recent,
            None,
            "remaining head must be detached from removed entries"
        );

        cache.clear_with(|| Ok(())).expect("finish clear");
        assert!(cache.entries.is_empty());
        assert_eq!(cache.used, 0);
        assert_eq!(cache.least_recent, None);
        assert_eq!(cache.most_recent, None);
    }

    #[test]
    fn cancelled_loose_replacement_restores_location_and_resolved_bases() {
        let oid = test_oid();
        let prior_location = location(41);
        let replacement = || {
            RefDeltaBase::Resolved(ResolvedPackObject {
                object: Arc::new(EncodedObject::new(ObjectType::Blob, vec![9])),
                oid,
                depth: 0,
                origin: None,
            })
        };

        let mut bases = RefDeltaBases::new();
        bases.insert_location(oid, prior_location);
        assert!(matches!(
            bases.replace_transactionally(oid, replacement(), || Err(GitError::Cancelled)),
            Err(GitError::Cancelled)
        ));
        assert!(matches!(
            bases.get(&oid),
            Some(RefDeltaBase::Location(location)) if *location == prior_location
        ));

        let prior_object = Arc::new(EncodedObject::new(ObjectType::Blob, vec![7]));
        bases.entries.insert(
            oid,
            RefDeltaBase::Resolved(ResolvedPackObject {
                object: Arc::clone(&prior_object),
                oid,
                depth: 7,
                origin: Some(prior_location),
            }),
        );
        assert!(matches!(
            bases.replace_transactionally(oid, replacement(), || Err(GitError::Cancelled)),
            Err(GitError::Cancelled)
        ));
        let Some(RefDeltaBase::Resolved(restored)) = bases.get(&oid) else {
            panic!("resolved base must be restored");
        };
        assert!(Arc::ptr_eq(&restored.object, &prior_object));
        assert_eq!(restored.depth, 7);
        assert_eq!(restored.origin, Some(prior_location));
    }

    #[test]
    fn allocator_failure_below_limit_is_not_reported_as_limit_rejection() {
        let mut decoder: BoundedPackDecoder<Vec<u8>> = BoundedPackDecoder {
            sources: Vec::new(),
            limits: PackReadLimits {
                max_delta_depth: 0,
                max_materialized_bytes: usize::MAX,
                max_cached_bytes: 0,
            },
            cache: ByteCache::new(0),
        };
        let mut stats = PackReadStats::start(0);
        let requested = (isize::MAX as usize).saturating_add(1);
        let error = decoder
            .allocate_body(requested, 0, None, CancelFlag::never(), &mut stats)
            .expect_err("Vec capacity overflow must be an allocation error");
        assert!(requested < decoder.limits.max_materialized_bytes);
        assert!(matches!(
            error,
            PackReadError::Allocation(PackAllocationError {
                requested: actual,
                active: 0,
                cached: 0,
            }) if actual == requested
        ));
    }
}
