//! Async ranged storage with bounded scratch and a separate blocking CPU stage.
//!
//! Keys identify immutable bytes in a caller-selected namespace. Authentication,
//! snapshot selection and transport belong to the embedding application. Index
//! checksum, entry CRC and final Git identity are verified; this is not a chunk
//! authentication format. No complete pack is downloaded or written locally.
use super::*;
use futures_util::{StreamExt, TryStreamExt, stream};
use parking_lot::Mutex;
use sley_pack::{
    BoundedPackDecoder, DeltaBase, PackDecodeBudget, PackIndex, PackObjectLocation, PackReadLimits,
    PackReadSource, RefDeltaBases, inspect_pack_entry,
};
use std::{collections::BTreeMap, future::Future, io, ops::Range};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Preserve decode limits, storage failures and blocking-worker failures separately.
#[derive(Debug)]
pub enum RemoteOdbError {
    Git(GitError),
    Decode(sley_pack::PackReadError),
    Worker(tokio::task::JoinError),
}
impl std::fmt::Display for RemoteOdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(error) => error.fmt(f),
            Self::Decode(error) => error.fmt(f),
            Self::Worker(error) => write!(f, "remote CPU worker failed: {error}"),
        }
    }
}
impl std::error::Error for RemoteOdbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Git(error) => error,
            Self::Decode(error) => error,
            Self::Worker(error) => error,
        })
    }
}
impl From<GitError> for RemoteOdbError {
    fn from(error: GitError) -> Self {
        Self::Git(error)
    }
}
impl From<sley_pack::PackReadError> for RemoteOdbError {
    fn from(error: sley_pack::PackReadError) -> Self {
        Self::Decode(error)
    }
}
impl From<tokio::task::JoinError> for RemoteOdbError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Worker(error)
    }
}
pub type RemoteOdbResult<T> = std::result::Result<T, RemoteOdbError>;

/// Exact ranged reads of immutable pack and index bytes. Short reads are errors.
/// Implementations must stop using `output` when their future is dropped.
/// Returned futures contain only I/O state; CPU decode runs on blocking workers.
pub trait RemoteOdbBackend: Send + Sync + 'static {
    fn read_range<'a>(
        &'a self,
        key: &'a str,
        range: Range<u64>,
        output: &'a mut [u8],
    ) -> impl Future<Output = Result<usize>> + Send + 'a;
}

/// Share across databases to cap simultaneous ranged requests and scratch bytes.
/// No mutex is held during I/O. Cancellation returns both the buffer and permit.
pub struct RemoteBufferPool {
    page_bytes: usize,
    slots: usize,
    available: Arc<Semaphore>,
    buffers: Mutex<Vec<Vec<u8>>>,
    cpu: Arc<Semaphore>,
}
impl RemoteBufferPool {
    pub fn new(page_bytes: usize, slots: usize) -> Result<Arc<Self>> {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        Self::with_cpu_workers(page_bytes, slots, workers)
    }
    /// Configure CPU concurrency independently of the number of I/O slots.
    pub fn with_cpu_workers(page_bytes: usize, slots: usize, workers: usize) -> Result<Arc<Self>> {
        if workers == 0
            || workers > Semaphore::MAX_PERMITS
            || page_bytes == 0
            || slots == 0
            || slots > Semaphore::MAX_PERMITS
            || page_bytes.checked_mul(slots).is_none()
        {
            return Err(GitError::InvalidFormat(
                "invalid remote buffer pool dimensions".into(),
            ));
        }
        let mut buffers = Vec::new();
        buffers.try_reserve_exact(slots).map_err(|error| {
            GitError::InvalidObject(format!("range pool allocation failed: {error}"))
        })?;
        buffers.resize_with(slots, Vec::new);
        Ok(Arc::new(Self {
            page_bytes,
            slots,
            available: Arc::new(Semaphore::new(slots)),
            buffers: Mutex::new(buffers),
            cpu: Arc::new(Semaphore::new(workers)),
        }))
    }
    pub fn capacity_bytes(&self) -> usize {
        self.page_bytes * self.slots
    }
    pub fn available_buffers(&self) -> usize {
        self.available.available_permits()
    }

    async fn acquire(self: &Arc<Self>) -> Result<RangeBuffer> {
        let permit = Arc::clone(&self.available)
            .acquire_owned()
            .await
            .map_err(|error| GitError::InvalidObject(format!("range pool closed: {error}")))?;
        let bytes = self.buffers.lock().pop().ok_or_else(|| {
            GitError::InvalidObject("range pool buffer accounting mismatch".into())
        })?;
        Ok(RangeBuffer {
            pool: Arc::clone(self),
            bytes,
            _permit: permit,
        })
    }

    async fn read<B: RemoteOdbBackend>(
        self: &Arc<Self>,
        backend: &B,
        key: &str,
        range: Range<u64>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let length = range
            .end
            .checked_sub(range.start)
            .and_then(|length| usize::try_from(length).ok())
            .filter(|length| *length <= limit)
            .ok_or_else(|| {
                GitError::InvalidObject("remote range exceeds encoded byte limit".into())
            })?;
        let mut output = Vec::new();
        output.try_reserve_exact(length).map_err(|error| {
            GitError::InvalidObject(format!("remote range allocation failed: {error}"))
        })?;
        let chunks = (0..length).step_by(self.page_bytes);
        let mut reads = stream::iter(chunks)
            .map(|start| async move {
                let size = self.page_bytes.min(length - start);
                let mut buffer = self.acquire().await?;
                if buffer.bytes.capacity() < size {
                    let additional = size - buffer.bytes.len();
                    buffer
                        .bytes
                        .try_reserve_exact(additional)
                        .map_err(|error| {
                            GitError::InvalidObject(format!(
                                "range buffer allocation failed: {error}"
                            ))
                        })?;
                }
                buffer.bytes.resize(size, 0);
                let offset = range.start + start as u64;
                let count = backend
                    .read_range(key, offset..offset + size as u64, &mut buffer.bytes)
                    .await?;
                if count != size {
                    return Err(GitError::InvalidObject(format!(
                        "short remote range: expected {size}, got {count}"
                    )));
                }
                Ok((start, buffer))
            })
            .buffered(self.slots);
        while let Some(result) = reads.next().await {
            let (_, buffer) = result?;
            output.extend_from_slice(&buffer.bytes);
        }
        Ok(output)
    }
}
struct RangeBuffer {
    pool: Arc<RemoteBufferPool>,
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for RangeBuffer {
    fn drop(&mut self) {
        self.pool
            .buffers
            .lock()
            .push(std::mem::take(&mut self.bytes));
    }
}

/// Explicit bounds for index metadata and each object's dependency closure.
#[derive(Clone, Copy, Debug)]
pub struct RemoteOdbLimits {
    pub max_index_bytes: usize,
    pub max_encoded_bytes: usize,
    pub decode: PackReadLimits,
}
impl Default for RemoteOdbLimits {
    fn default() -> Self {
        Self {
            max_index_bytes: 64 * 1024 * 1024,
            max_encoded_bytes: 64 * 1024 * 1024,
            decode: PackReadLimits {
                max_cached_bytes: 0,
                ..PackReadLimits::default()
            },
        }
    }
}

/// An immutable remote pack/index pair; lengths come from the caller's catalog.
pub struct RemotePackLocation {
    pub pack_key: String,
    pub pack_len: u64,
    pub index_key: String,
    pub index_len: u64,
    pub format: ObjectFormat,
}

/// A pack ODB that keeps index metadata and only fetches selected entry ranges.
/// Reads use Tokio for the blocking CPU handoff. Share the pool and byte budget
/// between handles. Caller-held objects and bounded index metadata are excluded
/// from the shared budget; encoded closures and CPU reservations are included.
pub struct RemoteObjectDatabase<B> {
    backend: Arc<B>,
    pool: Arc<RemoteBufferPool>,
    budget: Arc<PackDecodeBudget>,
    location: RemotePackLocation,
    index: PackIndex,
    offsets: BTreeMap<u64, (u64, ObjectId, u32)>,
    limits: RemoteOdbLimits,
}
impl<B: RemoteOdbBackend> RemoteObjectDatabase<B> {
    pub async fn open(
        backend: Arc<B>,
        pool: Arc<RemoteBufferPool>,
        budget: Arc<PackDecodeBudget>,
        location: RemotePackLocation,
        limits: RemoteOdbLimits,
    ) -> RemoteOdbResult<Self> {
        let index_bytes = pool
            .read(
                backend.as_ref(),
                &location.index_key,
                0..location.index_len,
                limits.max_index_bytes,
            )
            .await?;
        let end = location
            .pack_len
            .checked_sub(location.format.raw_len() as u64)
            .filter(|end| *end >= 12)
            .ok_or_else(|| GitError::InvalidFormat("remote pack is too short".into()))?;
        let format = location.format;
        let permit = Arc::clone(&pool.cpu)
            .acquire_owned()
            .await
            .map_err(|_| GitError::Cancelled)?;
        let (index, offsets) = tokio::task::spawn_blocking(move || -> Result<_> {
            let _permit = permit;
            let index = PackIndex::parse(&index_bytes, format)?;
            let mut ordered: Vec<_> = index.entries.iter().collect();
            ordered.sort_unstable_by_key(|entry| entry.offset);
            let mut offsets = BTreeMap::new();
            for (position, entry) in ordered.iter().enumerate() {
                let next = ordered
                    .get(position + 1)
                    .map(|entry| entry.offset)
                    .unwrap_or(end);
                if entry.offset < 12 || entry.offset >= next || next > end {
                    return Err(GitError::InvalidFormat("invalid remote index span".into()));
                }
                offsets.insert(entry.offset, (next, entry.oid, entry.crc32));
            }
            if ordered.first().map(|entry| entry.offset).unwrap_or(end) != 12 {
                return Err(GitError::InvalidFormat(
                    "remote index does not start at pack header".into(),
                ));
            }
            Ok((index, offsets))
        })
        .await??;
        let header = pool
            .read(backend.as_ref(), &location.pack_key, 0..12, 12)
            .await?;
        let trailer = pool
            .read(
                backend.as_ref(),
                &location.pack_key,
                end..location.pack_len,
                32,
            )
            .await?;
        if &header[..4] != b"PACK"
            || !matches!(
                u32::from_be_bytes([header[4], header[5], header[6], header[7]]),
                2 | 3
            )
            || u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize
                != index.entries.len()
            || trailer != index.pack_checksum.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "remote pack/index header or checksum mismatch".into(),
            )
            .into());
        }
        Ok(Self {
            backend,
            pool,
            budget,
            location,
            index,
            offsets,
            limits,
        })
    }
    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.index.find(oid).is_some()
    }

    /// Fetch independent requests concurrently; results retain input order.
    /// Each closure's range pages overlap in flight, also for a single object.
    pub async fn read_objects(
        &self,
        oids: &[ObjectId],
    ) -> RemoteOdbResult<Vec<Arc<EncodedObject>>> {
        stream::iter(oids)
            .map(|oid| self.read_object(oid))
            .buffered(self.pool.slots)
            .try_collect()
            .await
    }

    pub async fn read_object(&self, oid: &ObjectId) -> RemoteOdbResult<Arc<EncodedObject>> {
        let target = self
            .index
            .find(oid)
            .ok_or_else(|| GitError::object_not_found(*oid))?
            .offset;
        let mut offset = target;
        let mut ranges = BTreeMap::new();
        let mut locations = Vec::new();
        let mut encoded_bytes = 0usize;
        // Acquire one conservative reservation before fetching. Growing separate
        // partial reservations could deadlock when concurrent closures fill the
        // budget. A smaller budget is usable if the actual closure fits it.
        let decode_bytes = self.limits.decode.max_materialized_bytes;
        let reservation_bytes = self
            .limits
            .max_encoded_bytes
            .saturating_add(decode_bytes)
            .min(self.budget.limit_bytes());
        if decode_bytes > reservation_bytes {
            return Err(sley_pack::PackReadError::Limit(sley_pack::PackLimitError {
                kind: sley_pack::PackLimitKind::MaterializedBytes,
                limit: self.budget.limit_bytes(),
                attempted: decode_bytes,
            })
            .into());
        }
        let mut charge = self.budget.reserve_async(reservation_bytes).await?;
        loop {
            if ranges.contains_key(&offset) {
                return Err(GitError::InvalidObject("cycle in remote delta bases".into()).into());
            }
            let &(end, entry_oid, crc) = self.offsets.get(&offset).ok_or_else(|| {
                GitError::InvalidObject("remote delta base offset is not indexed".into())
            })?;
            let size = usize::try_from(end - offset)
                .map_err(|_| GitError::InvalidObject("remote entry length exceeds usize".into()))?;
            encoded_bytes = encoded_bytes
                .checked_add(size)
                .filter(|bytes| *bytes <= self.limits.max_encoded_bytes)
                .ok_or_else(|| {
                    GitError::InvalidObject("remote closure exceeds encoded byte limit".into())
                })?;
            let required = encoded_bytes.saturating_add(decode_bytes);
            if required > reservation_bytes {
                return Err(sley_pack::PackReadError::Limit(sley_pack::PackLimitError {
                    kind: sley_pack::PackLimitKind::MaterializedBytes,
                    limit: self.budget.limit_bytes(),
                    attempted: required,
                })
                .into());
            }
            let bytes = self
                .pool
                .read(
                    self.backend.as_ref(),
                    &self.location.pack_key,
                    offset..end,
                    size,
                )
                .await?;
            if crc32fast::hash(&bytes) != crc {
                return Err(GitError::InvalidObject("remote entry CRC mismatch".into()).into());
            }
            let (_, base) = inspect_pack_entry(&bytes, offset, self.location.format)?;
            ranges.insert(offset, bytes);
            locations.push((entry_oid, offset));
            let Some(base) = base else {
                break;
            };
            if ranges.len() > self.limits.decode.max_delta_depth {
                return Err(
                    GitError::InvalidObject("remote delta depth limit exceeded".into()).into(),
                );
            }
            offset = match base {
                DeltaBase::Offset(base) => base,
                DeltaBase::Ref(base) => {
                    self.index
                        .find(&base)
                        .ok_or_else(|| GitError::object_not_found(base))?
                        .offset
                }
                _ => {
                    return Err(
                        GitError::Unsupported("unknown remote delta base kind".into()).into(),
                    );
                }
            };
        }
        charge.resize(
            encoded_bytes
                .checked_add(self.limits.decode.max_materialized_bytes)
                .ok_or_else(|| {
                    GitError::InvalidObject("remote decode reservation overflow".into())
                })?,
        )?;
        let permit = Arc::clone(&self.pool.cpu)
            .acquire_owned()
            .await
            .map_err(|error| GitError::InvalidObject(format!("remote CPU pool closed: {error}")))?;
        let format = self.location.format;
        let length = self.location.pack_len;
        let limits = self.limits.decode;
        let oid = *oid;
        // Move all large state into the blocking closure, never an async stack.
        let (object, _charge) = tokio::task::spawn_blocking(move || -> RemoteOdbResult<_> {
            let _permit = permit;
            let source = FetchedRanges { length, ranges };
            let mut bases = RefDeltaBases::new();
            let mut decoder = BoundedPackDecoder::new(source, format, limits)?;
            for (oid, offset) in locations {
                bases.insert_location(
                    oid,
                    PackObjectLocation::new(decoder.primary_source(), offset),
                );
            }
            let outcome = decoder.read_object_at(target, &bases)?;
            if outcome.object_id() != oid {
                return Err(
                    GitError::InvalidObject("remote object identity mismatch".into()).into(),
                );
            }
            Ok((outcome.into_object(), charge))
        })
        .await??;
        Ok(object)
    }
}

struct FetchedRanges {
    length: u64,
    ranges: BTreeMap<u64, Vec<u8>>,
}
impl PackReadSource for FetchedRanges {
    fn len(&self) -> io::Result<u64> {
        Ok(self.length)
    }
    fn entry_end(&self, offset: u64) -> Option<u64> {
        self.ranges
            .get(&offset)
            .map(|bytes| offset + bytes.len() as u64)
    }
    fn read_at(&self, offset: u64, output: &mut [u8]) -> io::Result<usize> {
        let (&start, bytes) =
            self.ranges.range(..=offset).next_back().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "unfetched pack range")
            })?;
        let position = usize::try_from(offset - start).map_err(io::Error::other)?;
        let remaining = bytes
            .get(position..)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "unfetched pack range"))?;
        let count = output.len().min(remaining.len());
        output[..count].copy_from_slice(&remaining[..count]);
        Ok(count)
    }
}

/// In-memory immutable backend for embedding tests without a network service.
#[derive(Default)]
pub struct MemoryRemoteBackend {
    objects: BTreeMap<String, Arc<[u8]>>,
}
impl MemoryRemoteBackend {
    pub fn new(objects: impl IntoIterator<Item = (String, Arc<[u8]>)>) -> Self {
        Self {
            objects: objects.into_iter().collect(),
        }
    }
}
impl RemoteOdbBackend for MemoryRemoteBackend {
    async fn read_range(&self, key: &str, range: Range<u64>, output: &mut [u8]) -> Result<usize> {
        let bytes = self
            .objects
            .get(key)
            .ok_or_else(|| GitError::InvalidObject("missing remote key".into()))?;
        let start = usize::try_from(range.start)
            .map_err(|_| GitError::InvalidObject("invalid range start".into()))?;
        let end = usize::try_from(range.end)
            .map_err(|_| GitError::InvalidObject("invalid range end".into()))?;
        let selected = bytes
            .get(start..end)
            .filter(|bytes| bytes.len() == output.len())
            .ok_or_else(|| GitError::InvalidObject("remote range outside object".into()))?;
        output.copy_from_slice(selected);
        Ok(output.len())
    }
}
