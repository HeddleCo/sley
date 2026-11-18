//! Bounded, sequential discovery and parallel depth-first pack resolution.
//!
//! Visitors run on workers and borrow bodies only for the call. The live-base
//! gate counts retained ancestor bodies across all workers. A separate shared
//! in-flight gate reserves complete decoding workspaces before admitting workers.
//! Metadata is proportional to `max_objects`; zlib scratch is separate.
//! A full gate trades repeated decoding for memory; replay never calls visitors.
use super::*;
use flate2::{Decompress, FlushDecompress};
use sley_core::AtomicCancel;
use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
    mpsc,
};

/// Limits checked before reserving entry metadata, delta streams or bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveLimits {
    /// Maximum delta links; defaults to 50.
    pub max_delta_depth: usize,
    /// Header count ceiling, checked before metadata allocation; defaults to ten million.
    pub max_objects: usize,
    /// Maximum body or inflated delta stream size; defaults to 512 MiB.
    pub max_object_bytes: u64,
    /// Bounds logical results, inflated streams and decoded work (including
    /// replay), each independently across all workers; defaults to 16 GiB.
    /// Compressed work is additionally capped at input length plus this limit.
    pub max_total_decoded: u64,
    /// Shared retained ancestor-body budget; defaults to 64 MiB. Zero forces replay.
    pub max_live_base_bytes: usize,
    /// Shared transient workspace ceiling; defaults to 1 GiB. Workers wait for space.
    pub max_in_flight_bytes: usize,
    /// Discovery inflations plus every resolve/replay inflate or apply; defaults to 100M.
    pub max_resolve_ops: u64,
}
impl Default for ResolveLimits {
    fn default() -> Self {
        Self {
            max_delta_depth: MAX_READ_DELTA_CHAIN_DEPTH,
            max_objects: 10_000_000,
            max_object_bytes: 512 * 1024 * 1024,
            max_total_decoded: 16 * 1024 * 1024 * 1024,
            max_live_base_bytes: 64 * 1024 * 1024,
            max_in_flight_bytes: 1024 * 1024 * 1024,
            max_resolve_ops: 100_000_000,
        }
    }
}
impl ResolveLimits {
    /// Trusted legacy policy: no object, work or memory caps; retain the depth limit.
    pub fn unlimited() -> Self {
        Self {
            max_objects: usize::MAX,
            max_object_bytes: u64::MAX,
            max_total_decoded: u64::MAX,
            max_live_base_bytes: usize::MAX,
            max_in_flight_bytes: usize::MAX,
            max_resolve_ops: u64::MAX,
            ..Self::default()
        }
    }
}
/// Counters for one resolution. Use a fresh value for each call.
/// Workspace reservations conservatively cover all transient decoding buffers.
#[derive(Debug, Default)]
pub struct ResolveMetrics {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    ops: AtomicU64,
}
impl ResolveMetrics {
    pub fn in_flight_bytes(&self) -> usize {
        self.in_flight.load(AtomicOrdering::Acquire)
    }
    pub fn peak_in_flight_bytes(&self) -> usize {
        self.peak_in_flight.load(AtomicOrdering::Acquire)
    }
    pub fn resolve_ops(&self) -> u64 {
        self.ops.load(AtomicOrdering::Acquire)
    }
}
fn resolve_op<E>(metrics: &ResolveMetrics, limit: u64) -> std::result::Result<(), ResolveError<E>> {
    metrics
        .ops
        .try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |used| {
            used.checked_add(1).filter(|next| *next <= limit)
        })
        .map(|_| ())
        .map_err(|used| ResolveError::ResolveOps {
            limit,
            attempted: used.saturating_add(1),
        })
}
/// Verified, inflated Git delta instructions and their immediate resolved base.
#[derive(Debug)]
pub struct ResolvedDelta<'a> {
    pub base: ObjectId,
    pub base_offset: u64,
    pub instructions: &'a [u8],
}
/// One resolved pack entry. Order is unspecified; each entry is visited once.
#[derive(Debug)]
pub struct ResolvedObject<'a> {
    /// Plain SHA-1/SHA-256, NOT SHA-1 collision detection. For untrusted SHA-1
    /// input, re-hash canonical header + body with sha1dc and reject collisions
    /// or a digest mismatch before admitting this object.
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub body: &'a [u8],
    pub offset: u64,
    pub depth: u16,
    pub delta: Option<ResolvedDelta<'a>>,
}
/// The first pack or visitor error. Other workers stop cooperatively.
#[derive(Debug)]
pub enum ResolveError<E> {
    Pack(GitError),
    Visitor(E),
    /// Map to IMPORT_BUDGET_EXCEEDED{resolve_ops}; the attempted op did not run.
    ResolveOps {
        limit: u64,
        attempted: u64,
    },
}
impl<E: fmt::Display> fmt::Display for ResolveError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pack(e) => e.fmt(f),
            Self::Visitor(e) => write!(f, "pack visitor: {e}"),
            Self::ResolveOps { limit, attempted } => write!(
                f,
                "resolve_ops budget {limit} exceeded at operation {attempted}"
            ),
        }
    }
}
impl<E: std::error::Error + 'static> std::error::Error for ResolveError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Pack(e) => e,
            Self::Visitor(e) => e,
            Self::ResolveOps { .. } => return None,
        })
    }
}
impl<E> From<GitError> for ResolveError<E> {
    fn from(error: GitError) -> Self {
        Self::Pack(error)
    }
}
pub(crate) fn infallible_error(error: ResolveError<std::convert::Infallible>) -> GitError {
    match error {
        ResolveError::Pack(error) => error,
        ResolveError::Visitor(never) => match never {},
        ResolveError::ResolveOps { limit, attempted } => invalid(&format!(
            "resolve_ops budget {limit} exceeded at operation {attempted}"
        )),
    }
}
/// Resolve a self-contained pack, visiting each entry exactly once on workers.
///
/// A thin pack is an error. On any error the supplied cancel source is set.
/// `threads == 0` selects one worker. Output is independent of worker count.
/// Delta stream sizes are bounded as well as result sizes; the decoded budget
/// separately bounds discovery, inflated streams and whole/delta result work,
/// including replay. Exhausting a replay budget refuses the pack.
///
/// SHA-1 IDs use ordinary SHA-1, without collision detection. Admission callers
/// must run sha1dc over `type + " " + decimal_length + NUL + body` in `visit`,
/// compare its digest to `oid`, and reject collisions or mismatches there (the
/// FAST-IMPORT design assigns this check to heddle's admission hasher).
///
/// An error returns no index. Visitor calls already made cannot be rolled back:
/// keep callback effects provisional until this function returns `Ok`. Stage
/// writes/spool admissions and discard them on error, including a late thin pack.
///
/// Decoded buffers occupy at most `max_in_flight_bytes + max_live_base_bytes`
/// across ALL threads. RSS additionally includes the borrowed input, O(object
/// count, capped by `max_objects`) metadata/index, allocator slack, per-worker
/// zlib/thread scratch, and
/// callback-owned output (discovery uses a fixed 64 KiB scratch buffer). Actual
/// worker count also fits the gate to bound idle allocator arenas. Each worker
/// reserves a complete worst-case workspace
/// (base + instructions + result) before decoding, so waiting cannot deadlock
/// while holding transient buffers. A workspace larger than the gate is refused.
/// [`resolve_pack_with_metrics`] exposes the shared reservation/operation counters.
pub fn resolve_pack<V, E>(
    pack: &[u8],
    format: ObjectFormat,
    threads: usize,
    limits: ResolveLimits,
    cancel: CancelFlag<'_>,
    visit: &V,
) -> std::result::Result<PackIndexBuild, ResolveError<E>>
where
    V: Fn(&ResolvedObject<'_>) -> std::result::Result<(), E> + Sync,
    E: Send,
{
    resolve_pack_with_metrics(
        pack,
        format,
        threads,
        limits,
        cancel,
        visit,
        &ResolveMetrics::default(),
    )
}
/// [`resolve_pack`] with observable workspace and operation accounting.
pub fn resolve_pack_with_metrics<V, E>(
    pack: &[u8],
    format: ObjectFormat,
    threads: usize,
    limits: ResolveLimits,
    cancel: CancelFlag<'_>,
    visit: &V,
    metrics: &ResolveMetrics,
) -> std::result::Result<PackIndexBuild, ResolveError<E>>
where
    V: Fn(&ResolvedObject<'_>) -> std::result::Result<(), E> + Sync,
    E: Send,
{
    resolve_with_base(
        pack,
        format,
        threads,
        limits,
        cancel,
        visit,
        &mut |_| Ok(None),
        &mut |_| {},
        metrics,
    )
}

#[derive(Debug)]
struct Descriptor {
    offset: usize,
    data: usize,
    end: usize,
    header: EntryHeader,
    base: Option<DeltaBase>,
    result_size: u64,
}
fn invalid(message: &str) -> GitError {
    GitError::InvalidFormat(message.into())
}
fn size_check(size: u64, limits: ResolveLimits) -> Result<usize> {
    if size > limits.max_object_bytes {
        return Err(invalid("ResolveLimits::max_object_bytes exceeded"));
    }
    usize::try_from(size).map_err(|_| invalid("pack object size overflows usize"))
}
fn charge(total: &mut u64, size: u64, limits: ResolveLimits) -> Result<()> {
    *total = total
        .checked_add(size)
        .filter(|sum| *sum <= limits.max_total_decoded)
        .ok_or_else(|| invalid("ResolveLimits::max_total_decoded exceeded"))?;
    Ok(())
}
fn reserve<T>(size: usize) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(size)
        .map_err(|_| invalid("could not allocate pack buffer"))?;
    Ok(result)
}
/// Measure one zlib member without allocating its declared output. The first
/// twenty output bytes suffice for both delta varints, even across short reads.
fn measure(
    pack: &[u8],
    d: &mut Descriptor,
    limits: ResolveLimits,
    total: &mut u64,
    cancel: CancelFlag<'_>,
    scratch: &mut [u8],
) -> Result<()> {
    let mut checked = d.base.is_none();
    if checked {
        charge(total, d.header.size, limits)?;
        d.result_size = d.header.size;
    }
    let mut z = Decompress::new(true);
    let mut prefix = [0; 20];
    let mut prefix_len = 0;
    loop {
        cancel.check()?;
        let before = (z.total_in(), z.total_out());
        let status = z
            .decompress(
                &pack[d.data + before.0 as usize..],
                scratch,
                FlushDecompress::None,
            )
            .map_err(|_| invalid("invalid pack zlib stream"))?;
        if z.total_out() > d.header.size {
            return Err(invalid("pack declared size mismatch"));
        }
        if !checked {
            let produced = (z.total_out() - before.1) as usize;
            let copy = produced.min(prefix.len() - prefix_len);
            prefix[prefix_len..prefix_len + copy].copy_from_slice(&scratch[..copy]);
            prefix_len += copy;
            if prefix_len == prefix.len()
                || status == Status::StreamEnd
                || prefix[..prefix_len]
                    .iter()
                    .filter(|b| **b & 0x80 == 0)
                    .count()
                    >= 2
            {
                let size = decoded_delta_result_size(&prefix[..prefix_len])?;
                size_check(size, limits)?;
                charge(total, size, limits)?;
                d.result_size = size;
                checked = true;
            }
        }
        if status == Status::StreamEnd {
            if z.total_out() != d.header.size {
                return Err(invalid("pack declared size mismatch"));
            }
            d.end = d.data + z.total_in() as usize;
            return Ok(());
        }
        if before == (z.total_in(), z.total_out()) {
            return Err(invalid("truncated pack zlib stream"));
        }
    }
}

fn discover<E>(
    pack: &[u8],
    format: ObjectFormat,
    limits: ResolveLimits,
    cancel: CancelFlag<'_>,
    metrics: &ResolveMetrics,
) -> std::result::Result<(ObjectId, Vec<Descriptor>), ResolveError<E>> {
    cancel.check()?;
    if pack.len() < 12 + format.raw_len() || &pack[..4] != b"PACK" {
        return Err(invalid("truncated pack or missing PACK signature").into());
    }
    if !matches!(u32_be(&pack[4..8]), 2 | 3) {
        return Err(invalid("unsupported pack version").into());
    }
    let end = pack.len() - format.raw_len();
    let count = u32_be(&pack[8..12]) as usize;
    if count > limits.max_objects {
        return Err(invalid("ResolveLimits::max_objects exceeded").into());
    }
    checked_pack_object_count(count as u32, (end - 12) as u64)?;
    let mut digest = StreamingDigest::new(format);
    for chunk in pack[..end].chunks(64 * 1024) {
        cancel.check()?;
        digest.update(chunk);
    }
    let checksum = digest.finalize()?;
    if checksum.as_bytes() != &pack[end..] {
        return Err(invalid("pack checksum mismatch").into());
    }
    let mut entries = reserve(pack_entry_prealloc(count))?;
    // Fixed discovery scratch, independent of attacker-declared body sizes.
    let mut scratch = reserve(64 * 1024)?;
    scratch.resize(64 * 1024, 0);
    let (mut offset, mut work, mut total) = (12, 0, 0);
    for _ in 0..count {
        cancel.check()?;
        let mut cursor = offset;
        let header = parse_entry_header(&pack[..end], &mut cursor)?;
        size_check(header.size, limits)?;
        charge(&mut work, header.size, limits)?;
        let base = parse_entry_base(
            &pack[..end],
            &mut cursor,
            offset as u64,
            format,
            header.kind,
        )?;
        let mut d = Descriptor {
            offset,
            data: cursor,
            end: 0,
            header,
            base,
            result_size: 0,
        };
        resolve_op(metrics, limits.max_resolve_ops)?;
        measure(
            &pack[..end],
            &mut d,
            limits,
            &mut total,
            cancel,
            &mut scratch,
        )?;
        offset = d.end;
        if entries.len() == entries.capacity() {
            let extra = entries.len().max(1).min(count - entries.len());
            entries
                .try_reserve_exact(extra)
                .map_err(|_| invalid("could not grow pack metadata"))?;
        }
        entries.push(d);
    }
    if offset != end {
        return Err(invalid("pack has trailing bytes before checksum").into());
    }
    Ok((checksum, entries))
}

struct State<E> {
    children: Vec<Vec<usize>>,
    refs: BTreeMap<ObjectId, Vec<usize>>,
    output: Vec<Option<(PackIndexEntry, PackIndexedObject)>>,
    error: Option<ResolveError<E>>,
    inflated_work: u64,
    decoded_work: u64,
    compressed_work: u64,
}
struct Engine<'a, 'c, V, E> {
    pack: &'a [u8],
    entries: &'a [Descriptor],
    format: ObjectFormat,
    limits: ResolveLimits,
    cancel: CancelFlag<'c>,
    stop: AtomicCancel,
    visit: &'a V,
    state: Mutex<State<E>>,
    live: AtomicUsize,
    metrics: &'a ResolveMetrics,
    workspace: usize,
    flight_wait: Mutex<()>,
    flight_wake: Condvar,
}
/// Own the byte charge with its body, including error unwinds.
struct Retained<'a> {
    body: Vec<u8>,
    live: &'a AtomicUsize,
}
impl Drop for Retained<'_> {
    fn drop(&mut self) {
        let size = self.body.len();
        drop(std::mem::take(&mut self.body));
        self.live.fetch_sub(size, AtomicOrdering::AcqRel);
    }
}
// This lease is declared before any walk buffers and drops after all of them.
struct Flight<'a> {
    used: &'a AtomicUsize,
    wake: &'a Condvar,
    bytes: usize,
}
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, AtomicOrdering::AcqRel);
        self.wake.notify_all();
    }
}
struct Frame<'a> {
    index: Option<usize>,
    oid: ObjectId,
    object_type: ObjectType,
    offset: u64,
    depth: usize,
    body: Option<Retained<'a>>,
    children: std::vec::IntoIter<usize>,
}
#[derive(Clone, Copy)]
enum Root<'a> {
    Entry(usize),
    External(ObjectId, &'a EncodedObject),
}
enum Work {
    Inflated,
    Decoded,
    Compressed,
}
impl<V, E> Engine<'_, '_, V, E>
where
    V: Fn(&ResolvedObject<'_>) -> std::result::Result<(), E> + Sync,
    E: Send,
{
    fn check(&self) -> Result<()> {
        self.cancel.check()?;
        self.stop_flag().check()
    }
    fn stop_flag(&self) -> CancelFlag<'_> {
        CancelFlag::new(&self.stop)
    }
    fn fail(&self, error: ResolveError<E>) {
        if let Ok(mut state) = self.state.lock()
            && state.error.is_none()
        {
            state.error = Some(error);
        }
        self.stop.cancel();
        if let Some(source) = self.cancel.source() {
            source.cancel();
        }
    }
    // Replays must spend the same shared work budgets as first-time decoding;
    // otherwise a large discarded base with many tiny children amplifies CPU.
    fn work(&self, size: u64, kind: Work) -> Result<()> {
        self.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| invalid("pack state poisoned"))?;
        let limit = self.limits.max_total_decoded;
        let (used, limit) = match kind {
            Work::Inflated => (&mut state.inflated_work, limit),
            Work::Decoded => (&mut state.decoded_work, limit),
            // Empty DEFLATE blocks consume CPU but produce no decoded bytes.
            Work::Compressed => (
                &mut state.compressed_work,
                limit.saturating_add(self.pack.len() as u64),
            ),
        };
        charge(
            used,
            size,
            ResolveLimits {
                max_total_decoded: limit,
                ..self.limits
            },
        )
    }
    fn operation(&self) -> Result<()> {
        if let Err(error) = resolve_op(self.metrics, self.limits.max_resolve_ops) {
            self.fail(error);
            return Err(GitError::Cancelled);
        }
        Ok(())
    }
    fn flight(&self) -> Result<Flight<'_>> {
        let bytes = self.workspace;
        if bytes > self.limits.max_in_flight_bytes {
            return Err(invalid(
                "ResolveLimits::max_in_flight_bytes cannot fit workspace",
            ));
        }
        let mut wait = self
            .flight_wait
            .lock()
            .map_err(|_| invalid("pack gate poisoned"))?;
        loop {
            self.check()?;
            if let Ok(used) = self.metrics.in_flight.try_update(
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
                |used| {
                    used.checked_add(bytes)
                        .filter(|next| *next <= self.limits.max_in_flight_bytes)
                },
            ) {
                self.metrics
                    .peak_in_flight
                    .fetch_max(used + bytes, AtomicOrdering::AcqRel);
                return Ok(Flight {
                    used: &self.metrics.in_flight,
                    wake: &self.flight_wake,
                    bytes,
                });
            }
            // Timed waits also poll cancellation and tolerate a missed notification.
            wait = self
                .flight_wake
                .wait_timeout(wait, std::time::Duration::from_millis(10))
                .map_err(|_| invalid("pack gate poisoned"))?
                .0;
        }
    }
    fn inflate(&self, index: usize) -> Result<Vec<u8>> {
        self.check()?;
        self.operation()?;
        let d = &self.entries[index];
        let size = size_check(d.header.size, self.limits)?;
        self.work((d.end - d.data) as u64, Work::Compressed)?;
        self.work(d.header.size, Work::Inflated)?;
        if d.base.is_none() {
            self.work(d.header.size, Work::Decoded)?;
        }
        parallel_index::inflate_exact(&self.pack[d.data..d.end], size, self.stop_flag())
            .map(|pair| pair.0)
    }
    fn apply(&self, index: usize, base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
        self.check()?;
        self.operation()?;
        let plan = plan_pack_delta(base, delta)?;
        let size = size_check(plan.result_size, self.limits)?;
        self.work(plan.result_size, Work::Decoded)?;
        let mut body = reserve(size)?;
        apply_pack_delta_exact(base, delta, plan, &mut body, self.stop_flag())?;
        if body.len() as u64 != self.entries[index].result_size {
            return Err(invalid("delta result changed"));
        }
        Ok(body)
    }
    fn retain<'a>(&'a self, body: Vec<u8>) -> Option<Retained<'a>> {
        let size = body.len();
        self.live
            .try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |used| {
                used.checked_add(size)
                    .filter(|sum| *sum <= self.limits.max_live_base_bytes)
            })
            .ok()
            .map(|_| Retained {
                body,
                live: &self.live,
            })
    }
    fn replay<'a>(
        &self,
        frames: &'a [Frame<'_>],
        external: Option<&'a [u8]>,
    ) -> Result<std::borrow::Cow<'a, [u8]>> {
        let start = frames.iter().rposition(|f| f.body.is_some()).unwrap_or(0);
        let first = &frames[start];
        let mut body = match &first.body {
            Some(retained) => std::borrow::Cow::Borrowed(retained.body.as_slice()),
            None => match first.index {
                Some(index) => std::borrow::Cow::Owned(self.inflate(index)?),
                None => std::borrow::Cow::Borrowed(
                    external.ok_or_else(|| invalid("missing external base"))?,
                ),
            },
        };
        for frame in &frames[start + 1..] {
            let index = frame.index.ok_or_else(|| invalid("missing replay entry"))?;
            let delta = self.inflate(index)?;
            body = std::borrow::Cow::Owned(self.apply(index, &body, &delta)?);
        }
        Ok(body)
    }
    fn publish(
        &self,
        index: usize,
        body: &[u8],
        object_type: ObjectType,
        depth: usize,
        delta: Option<ResolvedDelta<'_>>,
        completed: &mpsc::SyncSender<()>,
    ) -> std::result::Result<Frame<'_>, ResolveError<E>> {
        self.check()?;
        let d = &self.entries[index];
        let oid = parallel_index::cancellable_object_id_bytes(
            object_type,
            body,
            self.format,
            self.stop_flag(),
        )?;
        (self.visit)(&ResolvedObject {
            oid,
            object_type,
            body,
            offset: d.offset as u64,
            depth: depth as u16,
            delta,
        })
        .map_err(ResolveError::Visitor)?;
        self.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| invalid("pack state poisoned"))?;
        let mut children = std::mem::take(&mut state.children[index]);
        if let Some(mut refs) = state.refs.remove(&oid) {
            children.append(&mut refs);
        }
        state.output[index] = Some((
            PackIndexEntry {
                oid,
                offset: d.offset as u64,
                crc32: crc32fast::hash(&self.pack[d.offset..d.end]),
            },
            PackIndexedObject {
                oid,
                object_type,
                offset: d.offset as u64,
                size: body.len() as u64,
            },
        ));
        drop(state);
        completed
            .send(())
            .map_err(|_| invalid("pack progress receiver closed"))?;
        Ok(Frame {
            index: Some(index),
            oid,
            object_type,
            offset: d.offset as u64,
            depth,
            body: None,
            children: children.into_iter(),
        })
    }
    fn walk(
        &self,
        root: Root<'_>,
        completed: &mpsc::SyncSender<()>,
    ) -> std::result::Result<(), ResolveError<E>> {
        let _flight = self.flight()?;
        let (mut frame, body, external) = match root {
            Root::Entry(index) => {
                let body = self.inflate(index)?;
                let ty = parallel_index::object_type_for_kind(self.entries[index].header.kind)?;
                (
                    self.publish(index, &body, ty, 0, None, completed)?,
                    Some(body),
                    None,
                )
            }
            Root::External(oid, object) => {
                let children = self
                    .state
                    .lock()
                    .map_err(|_| invalid("pack state poisoned"))?
                    .refs
                    .remove(&oid)
                    .unwrap_or_default();
                (
                    Frame {
                        index: None,
                        oid,
                        object_type: object.object_type,
                        offset: 0,
                        depth: 0,
                        body: None,
                        children: children.into_iter(),
                    },
                    None,
                    Some(object.body.as_slice()),
                )
            }
        };
        if frame.children.len() == 0 {
            return Ok(());
        }
        frame.body = body.and_then(|body| self.retain(body));
        let mut frames = vec![frame];
        while !frames.is_empty() {
            self.check()?;
            let last = frames.len() - 1;
            let Some(index) = frames[last].children.next() else {
                frames.pop();
                continue;
            };
            let parent = &frames[last];
            let depth = parent.depth + 1;
            parallel_index::check_delta_depth(
                self.entries[index].offset,
                depth,
                PackReadLimits {
                    max_delta_depth: self.limits.max_delta_depth.min(u16::MAX as usize),
                    ..PackReadLimits::default()
                },
            )?;
            let base = self.replay(&frames, external)?;
            let delta = self.inflate(index)?;
            let body = self.apply(index, &base, &delta)?;
            drop(base);
            let info = ResolvedDelta {
                base: parent.oid,
                base_offset: parent.offset,
                instructions: &delta,
            };
            let object_type = parent.object_type;
            // Its last immediate dependent is now resolved; release before visiting.
            if frames[last].children.len() == 0 {
                frames[last].body.take();
            }
            let mut child =
                self.publish(index, &body, object_type, depth, Some(info), completed)?;
            if child.children.len() != 0 {
                child.body = self.retain(body);
                frames.push(child);
            }
        }
        Ok(())
    }
    fn run<P: FnMut(PackIndexProgress)>(
        &self,
        roots: &[Root<'_>],
        threads: usize,
        progress: &mut P,
        count: &mut u64,
    ) {
        let cursor = AtomicUsize::new(0);
        // Also limit actual threads: otherwise serial admissions can leave a
        // separate allocator arena full of freed buffers on every idle worker.
        let slots = self
            .limits
            .max_in_flight_bytes
            .checked_div(self.workspace)
            .unwrap_or(usize::MAX)
            .max(1);
        let workers = threads.max(1).min(roots.len()).min(slots);
        let (send, recv) = mpsc::sync_channel(workers.max(1));
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..workers {
                let send = send.clone();
                let cursor = &cursor;
                let handle = std::thread::Builder::new().spawn_scoped(
                    scope,
                    sley_core::diagnostics::inherit(move || {
                        while !self.stop.is_cancelled() {
                            let index = cursor.fetch_add(1, AtomicOrdering::Relaxed);
                            let Some(root) = roots.get(index) else {
                                break;
                            };
                            if let Err(error) = self.walk(*root, &send) {
                                self.fail(error);
                                break;
                            }
                        }
                    }),
                );
                match handle {
                    Ok(handle) => handles.push(handle),
                    Err(error) => {
                        self.fail(GitError::from(error).into());
                        break;
                    }
                }
            }
            drop(send);
            loop {
                match recv.recv_timeout(std::time::Duration::from_millis(10)) {
                    Ok(()) => {
                        *count += 1;
                        progress(PackIndexProgress {
                            completed_objects: *count,
                            total_objects: self.entries.len() as u64,
                        });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if let Err(error) = self.cancel.check() {
                    self.fail(error.into());
                }
            }
            for handle in handles {
                if handle.join().is_err() {
                    self.fail(invalid("parallel pack worker panicked").into());
                }
            }
        });
    }
}

// The legacy API adds external-base and coordinator-progress hooks to the same engine.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_with_base<V, E, F, P>(
    pack: &[u8],
    format: ObjectFormat,
    threads: usize,
    limits: ResolveLimits,
    cancel: CancelFlag<'_>,
    visit: &V,
    external: &mut F,
    progress: &mut P,
    metrics: &ResolveMetrics,
) -> std::result::Result<PackIndexBuild, ResolveError<E>>
where
    V: Fn(&ResolvedObject<'_>) -> std::result::Result<(), E> + Sync,
    E: Send,
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    P: FnMut(PackIndexProgress),
{
    let result = (|| {
        let (checksum, entries) = discover(pack, format, limits, cancel, metrics)?;
        let max_body = entries.iter().map(|d| d.result_size).max().unwrap_or(0);
        let mut workspace = max_body;
        for d in entries.iter().filter(|d| d.base.is_some()) {
            workspace = workspace.max(
                max_body
                    .checked_add(d.result_size)
                    .and_then(|size| size.checked_add(d.header.size))
                    .ok_or_else(|| invalid("pack workspace size overflow"))?,
            );
        }
        let workspace =
            usize::try_from(workspace).map_err(|_| invalid("pack workspace overflows usize"))?;
        let mut children: Vec<Vec<usize>> = reserve(entries.len())?;
        children.resize_with(entries.len(), Vec::new);
        let mut refs = BTreeMap::<ObjectId, Vec<usize>>::new();
        let mut roots = Vec::new();
        for (index, d) in entries.iter().enumerate() {
            match d.base {
                None => roots.push(Root::Entry(index)),
                Some(DeltaBase::Ref(oid)) => refs.entry(oid).or_default().push(index),
                Some(DeltaBase::Offset(offset)) => {
                    let base = entries
                        .binary_search_by_key(&offset, |d| d.offset as u64)
                        .map_err(|_| invalid("ofs-delta base offset not found"))?;
                    if base >= index {
                        return Err(invalid("pack delta cycle detected").into());
                    }
                    children[base].push(index);
                }
            }
        }
        let mut output = reserve(entries.len())?;
        output.resize_with(entries.len(), || None);
        let engine = Engine {
            pack,
            entries: &entries,
            format,
            limits,
            cancel,
            stop: AtomicCancel::new(),
            visit,
            state: Mutex::new(State {
                children,
                refs,
                output,
                error: None,
                inflated_work: 0,
                decoded_work: 0,
                compressed_work: 0,
            }),
            live: AtomicUsize::new(0),
            metrics,
            workspace,
            flight_wait: Mutex::new(()),
            flight_wake: Condvar::new(),
        };
        let mut count = 0;
        if entries.is_empty() {
            progress(PackIndexProgress::default());
        }
        engine.run(&roots, threads, progress, &mut count);
        // The pending REF map only shrinks. Snapshot once: repeatedly searching
        // past missing bases would make a thin pack with distinct bases quadratic.
        let pending: Vec<_> = engine
            .state
            .lock()
            .map_err(|_| invalid("pack state poisoned"))?
            .refs
            .keys()
            .copied()
            .collect();
        for oid in pending {
            let mut state = engine
                .state
                .lock()
                .map_err(|_| invalid("pack state poisoned"))?;
            if let Some(error) = state.error.take() {
                return Err(error);
            }
            if !state.refs.contains_key(&oid) {
                continue;
            }
            drop(state);
            cancel.check()?;
            if let Some(object) = external(&oid)? {
                size_check(object.body.len() as u64, limits)?;
                if object.object_id(format)? != oid {
                    return Err(invalid("external delta base identity mismatch").into());
                }
                engine.run(
                    &[Root::External(oid, &object)],
                    threads,
                    progress,
                    &mut count,
                );
            }
        }
        {
            let mut state = engine
                .state
                .lock()
                .map_err(|_| invalid("pack state poisoned"))?;
            if let Some(error) = state.error.take() {
                return Err(error);
            }
        }
        if count != entries.len() as u64 {
            return Err(invalid("unresolved, cyclic, or external delta base").into());
        }
        cancel.check()?;
        let state = engine
            .state
            .into_inner()
            .map_err(|_| invalid("pack state poisoned"))?;
        let mut indexed = reserve(entries.len())?;
        let mut objects = reserve(entries.len())?;
        for entry in state.output {
            cancel.check()?;
            let (index, object) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;
            indexed.push(index);
            objects.push(object);
        }
        let index = PackIndex::write_v2(format, &indexed, &checksum)?;
        cancel.check()?;
        Ok(PackIndexBuild {
            index,
            pack_checksum: checksum,
            entries: indexed,
            objects,
        })
    })();
    if result.is_err()
        && let Some(source) = cancel.source()
    {
        source.cancel();
    }
    result
}
