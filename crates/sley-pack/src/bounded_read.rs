//! Bounded, positional decoding of individual pack entries.
//!
//! This module deliberately reuses the crate's entry-header, OFS-offset, and
//! delta-application parsers. It only supplies the random-access I/O and the
//! iterative chain planner around that authoritative grammar.

use super::*;
use flate2::{Decompress, FlushDecompress, Status};
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Maximum decoded object and delta bytes owned or actively used by the
    /// decoder at one time. Returned objects cease to count after the call.
    pub max_materialized_bytes: usize,
    /// Maximum decoded body bytes retained between calls. The effective cache
    /// ceiling is also capped by `max_materialized_bytes`.
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

/// Error returned by bounded targeted decoding.
#[derive(Debug)]
pub enum PackReadError {
    Limit(PackLimitError),
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
            Self::Limit(_) => None,
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

/// Resolution supplied for a ref-delta base.
#[derive(Debug, Clone)]
pub enum RefDeltaBase {
    /// The base is another entry in this decoder's source. Returning its offset
    /// keeps REF chains on the decoder's iterative, depth-limited path.
    InPack(u64),
    /// The base lives outside this pack (or was already decoded by the caller).
    External(Arc<EncodedObject>),
}

/// Usage measured for one targeted read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackReadStats {
    /// Source bytes fetched while inflating zlib members. This counts positional
    /// read chunks, including any unread tail in the chunk containing StreamEnd.
    pub compressed_bytes_read: u64,
    /// Highest simultaneous decoded body/delta byte total during this call,
    /// including decoder cache contents.
    pub peak_materialized_bytes: usize,
    /// Decoded body bytes retained after this call.
    pub cached_bytes: usize,
    pub cached_objects: usize,
    pub cache_evictions: u64,
    /// Number of deltas resolved for the requested object.
    pub delta_depth: usize,
}

/// One decoded object and the resources measured while producing it.
#[derive(Debug, Clone)]
pub struct PackReadOutcome {
    pub object: Arc<EncodedObject>,
    pub stats: PackReadStats,
}

#[derive(Debug)]
struct CachedObject {
    object: Arc<EncodedObject>,
    bytes: usize,
}

#[derive(Debug)]
struct ByteCache {
    budget: usize,
    used: usize,
    entries: HashMap<u64, CachedObject>,
    recency: VecDeque<u64>,
    evictions: u64,
}

impl ByteCache {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            entries: HashMap::new(),
            recency: VecDeque::new(),
            evictions: 0,
        }
    }

    fn get(&mut self, offset: u64) -> Option<Arc<EncodedObject>> {
        let object = Arc::clone(&self.entries.get(&offset)?.object);
        self.touch(offset);
        Some(object)
    }

    fn take(&mut self, offset: u64) -> Option<Arc<EncodedObject>> {
        let cached = self.entries.remove(&offset)?;
        self.used = self.used.saturating_sub(cached.bytes);
        self.recency.retain(|candidate| *candidate != offset);
        Some(cached.object)
    }

    fn insert(&mut self, offset: u64, object: Arc<EncodedObject>) {
        let bytes = object.body.len();
        if bytes > self.budget || self.budget == 0 {
            return;
        }
        if let Some(previous) = self.entries.remove(&offset) {
            self.used = self.used.saturating_sub(previous.bytes);
            self.recency.retain(|candidate| *candidate != offset);
        }
        while self.used.saturating_add(bytes) > self.budget {
            if !self.evict_one() {
                return;
            }
        }
        self.used += bytes;
        self.entries.insert(offset, CachedObject { object, bytes });
        self.recency.push_back(offset);
    }

    fn touch(&mut self, offset: u64) {
        self.recency.retain(|candidate| *candidate != offset);
        self.recency.push_back(offset);
    }

    fn evict_one(&mut self) -> bool {
        let Some(offset) = self.recency.pop_front() else {
            return false;
        };
        if let Some(cached) = self.entries.remove(&offset) {
            self.used = self.used.saturating_sub(cached.bytes);
            self.evictions += 1;
        }
        true
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
        self.used = 0;
    }
}

#[derive(Debug)]
struct EntryPlan {
    header: EntryHeader,
    data_offset: u64,
    base: Option<DeltaBase>,
}

/// A targeted decoder tied to one pack source.
///
/// Delta chains are planned in a heap vector and resolved from base to target,
/// so call-stack use is constant with respect to attacker-controlled depth.
/// The internal cache is scoped to this decoder/source pair; do not replace the
/// bytes behind a custom source while a decoder is alive. Cache accounting is
/// by decoded body bytes, not entry count, and [`Self::clear_cache`] releases
/// every decoder-held object.
pub struct BoundedPackDecoder<S> {
    source: S,
    format: ObjectFormat,
    limits: PackReadLimits,
    trailer_offset: u64,
    cache: ByteCache,
}

impl<S: PackReadSource> BoundedPackDecoder<S> {
    pub fn new(
        source: S,
        format: ObjectFormat,
        limits: PackReadLimits,
    ) -> std::result::Result<Self, PackReadError> {
        let source_len = source.len()?;
        let trailer_len = format.raw_len() as u64;
        let trailer_offset = source_len
            .checked_sub(trailer_len)
            .ok_or_else(|| GitError::InvalidFormat("pack smaller than its trailer".into()))?;
        Ok(Self {
            source,
            format,
            limits,
            trailer_offset,
            cache: ByteCache::new(limits.max_cached_bytes.min(limits.max_materialized_bytes)),
        })
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

    /// Drop all decoded objects retained between targeted reads.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Decode the entry at `offset` without loading the whole pack.
    ///
    /// `resolve_ref_base` may return an in-pack offset to keep a REF chain in
    /// this decoder, an external decoded object, or `None` for a missing base.
    pub fn read_object_at<F>(
        &mut self,
        offset: u64,
        mut resolve_ref_base: F,
    ) -> std::result::Result<PackReadOutcome, PackReadError>
    where
        F: FnMut(&ObjectId) -> Result<Option<RefDeltaBase>>,
    {
        let evictions_before = self.cache.evictions;
        let mut stats = PackReadStats {
            peak_materialized_bytes: self.cache.used,
            ..PackReadStats::default()
        };
        if let Some(object) = self.cache.get(offset) {
            self.finish_stats(&mut stats, evictions_before);
            return Ok(PackReadOutcome { object, stats });
        }

        let mut visited = HashSet::new();
        let mut deltas = Vec::new();
        let mut current_offset = offset;
        let mut base_object = None;
        let mut base_entry = None;

        loop {
            if !visited.insert(current_offset) {
                return Err(GitError::InvalidFormat("pack delta cycle detected".into()).into());
            }
            if current_offset != offset
                && let Some(object) = self.cache.take(current_offset)
            {
                base_object = Some(object);
                break;
            }
            let entry = self.read_entry_plan(current_offset)?;
            match entry.base.clone() {
                None => {
                    base_entry = Some(entry);
                    break;
                }
                Some(base) => {
                    let depth = deltas.len().saturating_add(1);
                    if depth > self.limits.max_delta_depth {
                        return Err(PackReadError::Limit(PackLimitError {
                            kind: PackLimitKind::DeltaDepth,
                            limit: self.limits.max_delta_depth,
                            attempted: depth,
                        }));
                    }
                    deltas.push(entry);
                    match base {
                        DeltaBase::Offset(base_offset) => current_offset = base_offset,
                        DeltaBase::Ref(base_oid) => match resolve_ref_base(&base_oid)? {
                            Some(RefDeltaBase::InPack(base_offset)) => {
                                current_offset = base_offset;
                            }
                            Some(RefDeltaBase::External(object)) => {
                                base_object = Some(object);
                                break;
                            }
                            None => {
                                return Err(GitError::not_found(format!(
                                    "ref-delta base object {base_oid}"
                                ))
                                .into());
                            }
                        },
                    }
                }
            }
        }

        stats.delta_depth = deltas.len();
        let mut object = match (base_object, base_entry) {
            (Some(object), None) => {
                self.ensure_materialized(0, object.body.len(), &mut stats)?;
                object
            }
            (None, Some(entry)) => {
                let object_type = object_type_for_entry(entry.header.kind)?;
                let body = self.inflate_entry(&entry, 0, &mut stats)?;
                Arc::new(EncodedObject::new(object_type, body))
            }
            _ => {
                return Err(
                    GitError::InvalidFormat("pack delta base planning failed".into()).into(),
                );
            }
        };

        for delta_entry in deltas.iter().rev() {
            let base_bytes = object.body.len();
            let delta = self.inflate_entry(delta_entry, base_bytes, &mut stats)?;
            let result_size_u64 = decoded_delta_result_size(&delta)?;
            let result_size = usize::try_from(result_size_u64).map_err(|_| {
                PackReadError::Limit(PackLimitError {
                    kind: PackLimitKind::MaterializedBytes,
                    limit: self.limits.max_materialized_bytes,
                    attempted: usize::MAX,
                })
            })?;
            self.ensure_materialized(
                base_bytes.saturating_add(delta.len()),
                result_size,
                &mut stats,
            )?;
            let resolved = apply_pack_delta(&object.body, &delta)?;
            object = Arc::new(EncodedObject::new(object.object_type, resolved));
        }

        self.cache.insert(offset, Arc::clone(&object));
        self.finish_stats(&mut stats, evictions_before);
        Ok(PackReadOutcome { object, stats })
    }

    fn finish_stats(&self, stats: &mut PackReadStats, evictions_before: u64) {
        stats.cached_bytes = self.cache.used;
        stats.cached_objects = self.cache.entries.len();
        stats.cache_evictions = self.cache.evictions.saturating_sub(evictions_before);
        stats.peak_materialized_bytes = stats.peak_materialized_bytes.max(self.cache.used);
    }

    fn read_entry_plan(&self, offset: u64) -> std::result::Result<EntryPlan, PackReadError> {
        if offset >= self.trailer_offset {
            return Err(GitError::InvalidFormat("pack object offset out of range".into()).into());
        }
        let available =
            usize::try_from((self.trailer_offset - offset).min(ENTRY_PREFIX_BYTES as u64))
                .unwrap_or(ENTRY_PREFIX_BYTES);
        let mut prefix = [0u8; ENTRY_PREFIX_BYTES];
        self.read_exact_at(offset, &mut prefix[..available])?;
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
                let raw_len = self.format.raw_len();
                let end = cursor.checked_add(raw_len).ok_or_else(|| {
                    GitError::InvalidFormat("ref-delta base offset overflow".into())
                })?;
                let raw = bytes.get(cursor..end).ok_or_else(|| {
                    GitError::InvalidFormat("truncated ref-delta base object id".into())
                })?;
                cursor = end;
                Some(DeltaBase::Ref(ObjectId::from_raw(self.format, raw)?))
            }
            _ => None,
        };
        let data_offset = offset
            .checked_add(cursor as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack object offset overflow".into()))?;
        Ok(EntryPlan {
            header,
            data_offset,
            base,
        })
    }

    fn inflate_entry(
        &mut self,
        entry: &EntryPlan,
        active_bytes: usize,
        stats: &mut PackReadStats,
    ) -> std::result::Result<Vec<u8>, PackReadError> {
        let expected = usize::try_from(entry.header.size).map_err(|_| {
            PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.limits.max_materialized_bytes,
                attempted: usize::MAX,
            })
        })?;
        self.ensure_materialized(active_bytes, expected, stats)?;
        let mut body = Vec::new();
        body.try_reserve_exact(expected).map_err(|error| {
            GitError::InvalidObject(format!("pack inflate allocation failed: {error}"))
        })?;

        let mut decompressor = Decompress::new(true);
        let mut input = [0u8; INFLATE_CHUNK_BYTES];
        let mut output = [0u8; INFLATE_CHUNK_BYTES];
        let mut input_start = 0usize;
        let mut input_end = 0usize;
        let mut source_offset = entry.data_offset;

        loop {
            if input_start == input_end {
                if source_offset >= self.trailer_offset {
                    return Err(GitError::InvalidObject("truncated zlib stream".into()).into());
                }
                let wanted = usize::try_from(
                    (self.trailer_offset - source_offset).min(INFLATE_CHUNK_BYTES as u64),
                )
                .unwrap_or(INFLATE_CHUNK_BYTES);
                let read = self.source.read_at(source_offset, &mut input[..wanted])?;
                if read == 0 {
                    return Err(GitError::InvalidObject("truncated zlib stream".into()).into());
                }
                source_offset = source_offset
                    .checked_add(read as u64)
                    .ok_or_else(|| GitError::InvalidFormat("pack source offset overflow".into()))?;
                stats.compressed_bytes_read =
                    stats.compressed_bytes_read.saturating_add(read as u64);
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
        stats: &mut PackReadStats,
    ) -> std::result::Result<(), PackReadError> {
        let working = active_bytes.saturating_add(additional_bytes);
        if working > self.limits.max_materialized_bytes {
            return Err(PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.limits.max_materialized_bytes,
                attempted: working,
            }));
        }
        while self.cache.used.saturating_add(working) > self.limits.max_materialized_bytes {
            if !self.cache.evict_one() {
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

    fn read_exact_at(
        &self,
        mut offset: u64,
        mut buf: &mut [u8],
    ) -> std::result::Result<(), PackReadError> {
        while !buf.is_empty() {
            let read = self.source.read_at(offset, buf)?;
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
