//! Bounded, sequential discovery and parallel work-stealing pack resolution.
//!
//! Visitors run on workers and borrow bodies only for the call. The live-base
//! gate counts retained ancestor bodies across all workers. A separate shared
//! in-flight gate reserves complete decoding workspaces before admitting workers.
//! Metadata is proportional to `max_objects`; zlib scratch is separate.
//! A full gate trades repeated decoding for memory; replay never calls visitors.
use super::*;
use flate2::{Decompress, FlushDecompress};
use sley_core::{AtomicCancel, ResourceLimitKind};
use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
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
    /// Shared budget for fan-out bases awaiting siblings; defaults to 64 MiB. Zero disables caching.
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
    fn is_unlimited(self) -> bool {
        self == Self {
            max_delta_depth: self.max_delta_depth,
            ..Self::unlimited()
        }
    }
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
    metadata: AtomicUsize,
    live: AtomicUsize,
    peak_live: AtomicUsize,
}
impl ResolveMetrics {
    pub fn live_base_bytes(&self) -> usize {
        self.live.load(AtomicOrdering::Acquire)
    }
    pub fn peak_live_base_bytes(&self) -> usize {
        self.peak_live.load(AtomicOrdering::Acquire)
    }
    pub fn in_flight_bytes(&self) -> usize {
        self.in_flight.load(AtomicOrdering::Acquire)
    }
    pub fn peak_in_flight_bytes(&self) -> usize {
        self.peak_in_flight.load(AtomicOrdering::Acquire)
    }
    /// Conservative peak metadata capacity (vectors, REF slots, conversion/index
    /// scratch); excludes fixed worker/zlib state and allocator overhead.
    pub fn peak_metadata_bytes(&self) -> usize {
        self.metadata.load(AtomicOrdering::Acquire)
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
    /// Plain SHA-1/SHA-256 without collision detection. Untrusted SHA-1 callers
    /// must re-hash with sha1dc before admission; see [`resolve_pack`].
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub body: &'a [u8],
    pub offset: u64,
    pub depth: u16,
    pub delta: Option<ResolvedDelta<'a>>,
}
/// The first pack or visitor error. Other workers stop cooperatively.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResolveError<E> {
    Pack(GitError),
    Visitor(E),
    /// A worker or coordinator panicked; distinct from cooperative cancellation.
    Panic,
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
            Self::Panic => write!(f, "pack resolver callback panicked"),
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
            Self::ResolveOps { .. } | Self::Panic => return None,
        })
    }
}
impl<E> From<GitError> for ResolveError<E> {
    fn from(error: GitError) -> Self {
        Self::Pack(error)
    }
}
pub(crate) fn into_git_error<E>(e: ResolveError<E>, visit: impl FnOnce(E) -> GitError) -> GitError {
    match e {
        ResolveError::Pack(error) => error,
        ResolveError::Visitor(error) => visit(error),
        ResolveError::Panic => invalid("pack resolver callback panicked"),
        ResolveError::ResolveOps { limit, attempted } => {
            budget(ResourceLimitKind::ResolveOps, limit, attempted)
        }
    }
}
/// Resolve a self-contained pack, visiting each entry exactly once on workers.
///
/// A thin pack is an error. On any error the supplied cancel source is set.
/// `threads == 0` selects one worker. Index bytes are independent of worker count.
/// Depth-limit acceptance uses the shallowest duplicate OID; pack offset breaks
/// ties. Visitor depth/base offsets describe the actual resolution path. Depth
/// refusals wait for shallower duplicates before any over-limit delta is applied.
/// Delta stream sizes are bounded as well as result sizes; the decoded budget
/// separately bounds discovery, inflated streams and whole/delta result work,
/// including replay. A worker keeps each next DFS base inside its reserved
/// workspace, so a chain never replays; only siblings of a fan-out base that the
/// live gate could not hold replay, at most `max_delta_depth` ancestors each.
///
/// SHA-1 IDs use ordinary SHA-1, without collision detection. Admission callers
/// must run sha1dc over `type + " " + decimal_length + NUL + body` in `visit`,
/// compare its digest to `oid`, and reject collisions or mismatches there (the
/// admission contract assigns this check to the caller).
///
/// An error returns no index. Visitor calls already made cannot be rolled back:
/// keep callback effects provisional until this function returns `Ok`. Stage
/// writes/spool admissions and discard them on error, including a late thin pack.
///
/// Decoded buffers occupy at most `max_in_flight_bytes + max_live_base_bytes`
/// across ALL threads. RSS additionally includes the borrowed input, O(object
/// count, capped by `max_objects`) metadata/index, allocator slack, per-worker
/// zlib/thread scratch, O(threads * depth) scheduling nodes, and
/// callback-owned output (discovery uses a fixed 64 KiB scratch buffer). Actual
/// worker count also fits the gate to bound idle allocator arenas. Each worker
/// reserves a complete worst-case workspace
/// (base + instructions + result) before decoding, so waiting cannot deadlock
/// while holding transient buffers. The gate must fit `2 * largest result body +
/// largest delta stream` (sufficiently, `3 * max_object_bytes`); otherwise a typed
/// `ResourceLimitKind::ResolveInFlight` error refuses the pack before decoding.
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
    let result = resolve_with_base(
        pack,
        format,
        threads,
        limits,
        cancel,
        visit,
        &mut |_| Ok(None),
        &mut |_| {},
        metrics,
    );
    if result.is_err()
        && let Some(source) = cancel.source()
    {
        source.cancel();
    }
    result
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
fn budget(kind: ResourceLimitKind, limit: u64, attempted: u64) -> GitError {
    GitError::ResourceLimit {
        kind,
        limit,
        attempted,
    }
}
fn invalid(message: &str) -> GitError {
    GitError::InvalidFormat(message.into())
}
fn size_check(size: u64, limits: ResolveLimits) -> Result<usize> {
    let limit = limits.max_object_bytes.min(usize::MAX as u64);
    if size > limit {
        return Err(budget(ResourceLimitKind::ResolveObjectBytes, limit, size));
    }
    Ok(size as usize)
}
fn charge(total: &mut u64, size: u64, limit: u64) -> Result<()> {
    if limit == u64::MAX {
        return Ok(());
    }
    let attempted = total.saturating_add(size);
    if attempted > limit {
        return Err(budget(
            ResourceLimitKind::ResolveTotalDecoded,
            limit,
            attempted,
        ));
    }
    *total = attempted;
    Ok(())
}
fn allocation<T>(v: &Vec<T>) -> usize {
    v.capacity() * std::mem::size_of::<T>()
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
    z: &mut Decompress,
) -> Result<()> {
    let mut checked = d.base.is_none();
    if checked {
        charge(total, d.header.size, limits.max_total_decoded)?;
        d.result_size = d.header.size;
    }
    z.reset(true);
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
                charge(total, size, limits.max_total_decoded)?;
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
    threads: usize,
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
        return Err(budget(
            ResourceLimitKind::ResolveObjects,
            limits.max_objects as u64,
            count as u64,
        )
        .into());
    }
    let entries = reserve(pack_entry_prealloc(count, end - 12))?;
    metrics
        .metadata
        .fetch_max(allocation(&entries), AtomicOrdering::Relaxed);
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
    #[cfg(feature = "fetch-profile")]
    let _span = sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::Inflate);
    if limits.is_unlimited() {
        if count != 0 {
            parse_entry_header(&pack[..end], &mut 12)?;
        }
        return Ok((
            checksum,
            discover_trusted(&pack[..end], format, count, threads, cancel, entries)?,
        ));
    }
    let mut entries = entries;
    // Fixed discovery scratch, independent of attacker-declared body sizes.
    let mut scratch = reserve(64 * 1024)?;
    scratch.resize(64 * 1024, 0);
    let mut z = Decompress::new(true);
    let (mut offset, mut work, mut total) = (12, 0, 0);
    for _ in 0..count {
        cancel.check()?;
        let mut cursor = offset;
        let header = parse_entry_header(&pack[..end], &mut cursor)?;
        size_check(header.size, limits)?;
        charge(&mut work, header.size, limits.max_total_decoded)?;
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
            &mut z,
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

// Only an explicitly unlimited policy may speculate; bounded discovery never calls this.
fn discover_trusted(
    pack: &[u8],
    format: ObjectFormat,
    count: usize,
    threads: usize,
    cancel: CancelFlag<'_>,
    mut entries: Vec<Descriptor>,
) -> Result<Vec<Descriptor>> {
    let workers = threads.max(1).min(count.max(1));
    let chunk = (pack.len() - 12).div_ceil(workers).max(1);
    let candidates = std::thread::scope(|scope| -> Result<Vec<Descriptor>> {
        let mut handles = Vec::new();
        for start in (12..pack.len()).step_by(chunk) {
            handles.push(std::thread::Builder::new().spawn_scoped(
                scope,
                sley_core::diagnostics::inherit(move || -> Result<Vec<Descriptor>> {
                    let mut found = Vec::new();
                    let mut scratch = reserve(64 * 1024)?;
                    scratch.resize(64 * 1024, 0);
                    let mut z = Decompress::new(true);
                    for offset in start..(start + chunk).min(pack.len()) {
                        if offset & 4095 == 0 {
                            cancel.check()?;
                        }
                        let candidate = (|| {
                            let mut data = offset;
                            let header = parse_entry_header(pack, &mut data).ok()?;
                            let base = parse_entry_base(
                                pack,
                                &mut data,
                                offset as u64,
                                format,
                                header.kind,
                            )
                            .ok()?;
                            let bytes = pack.get(data..data.checked_add(2)?)?;
                            if bytes[0] & 15 != 8
                                || bytes[0] >> 4 > 7
                                || bytes[1] & 32 != 0
                                || !u16::from_be_bytes([bytes[0], bytes[1]]).is_multiple_of(31)
                            {
                                return None;
                            }
                            Some(Descriptor {
                                offset,
                                data,
                                end: 0,
                                header,
                                base,
                                result_size: 0,
                            })
                        })();
                        if let Some(mut d) = candidate
                            && measure(
                                pack,
                                &mut d,
                                ResolveLimits::unlimited(),
                                &mut 0,
                                cancel,
                                &mut scratch,
                                &mut z,
                            )
                            .is_ok()
                        {
                            found.push(d);
                        }
                    }
                    cancel.check()?;
                    Ok(found)
                }),
            )?);
        }
        let mut found = Vec::new();
        for handle in handles {
            found.append(
                &mut handle
                    .join()
                    .map_err(|_| invalid("pack discovery worker panicked"))??,
            );
        }
        Ok(found)
    })?;
    let mut offset = 12;
    for d in candidates {
        if d.offset == offset && entries.len() < count {
            offset = d.end;
            if entries.len() == entries.capacity() {
                entries
                    .try_reserve_exact(entries.len().max(1).min(count - entries.len()))
                    .map_err(|_| invalid("could not grow pack metadata"))?;
            }
            entries.push(d);
        }
    }
    if entries.len() != count || offset != pack.len() {
        return Err(invalid("invalid or truncated pack entries"));
    }
    Ok(entries)
}

type Output = (ObjectId, ObjectType, u32, u16);
struct State<E> {
    children: Vec<Vec<u32>>,
    refs: Vec<(ObjectId, Vec<u32>, Option<u32>)>,
    output: Vec<Option<Output>>,
    error: Option<ResolveError<E>>,
    /// Compressed input, inflated streams and decoded results charged so far.
    work: [u64; 3],
    deferred: Vec<u32>,
}
impl<E> State<E> {
    fn ref_slot(&self, oid: &ObjectId) -> Option<usize> {
        self.refs.binary_search_by_key(oid, |entry| entry.0).ok()
    }
    fn take_refs(&mut self, oid: &ObjectId) -> Vec<u32> {
        self.ref_slot(oid)
            .map(|i| std::mem::take(&mut self.refs[i].1))
            .unwrap_or_default()
    }
    // REF bases use the shallowest published duplicate; pack index breaks ties.
    fn prefer(&mut self, oid: &ObjectId, index: usize, depth: u16) {
        if let Some(slot) = self.ref_slot(oid)
            && self.refs[slot].2.is_none_or(|b| {
                self.output[b as usize].is_some_and(|o| (depth, index) < (o.3, b as usize))
            })
        {
            self.refs[slot].2 = Some(index as u32);
        }
    }
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
    metrics: &'a ResolveMetrics,
    workspace: usize,
    metadata_base: usize,
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
// This lease is declared before any worker buffers and drops after all of them.
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
struct Node<'a> {
    index: Option<u32>,
    oid: ObjectId,
    object_type: ObjectType,
    offset: u64,
    depth: u16,
    parent: Option<Arc<Node<'a>>>,
    children: Mutex<std::vec::IntoIter<u32>>,
    pending: AtomicU32,
    body: Mutex<Option<Arc<Retained<'a>>>>,
    external: Option<&'a [u8]>,
}
// Custom depth limits may exceed a worker's call stack. Drain unique parents
// iteratively; shared ancestors are drained by their eventual last owner.
impl Drop for Node<'_> {
    fn drop(&mut self) {
        let mut parent = self.parent.take();
        while let Some(node) = parent {
            parent = Arc::into_inner(node).and_then(|mut node| node.parent.take());
        }
    }
}
impl Node<'_> {
    fn release(&self) -> Result<()> {
        if self.pending.fetch_sub(1, AtomicOrdering::AcqRel) == 1 {
            self.body
                .lock()
                .map_err(|_| invalid("pack body poisoned"))?
                .take();
        }
        Ok(())
    }
}
enum Base<'a> {
    Retained(Arc<Retained<'a>>),
    Owned(Vec<u8>),
    External(&'a [u8]),
}
impl AsRef<[u8]> for Base<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Retained(body) => &body.body,
            Self::Owned(body) => body,
            Self::External(body) => body,
        }
    }
}
struct UnwindStop<F: Fn()>(F);
impl<F: Fn()> Drop for UnwindStop<F> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            (self.0)();
        }
    }
}
#[derive(Clone, Copy)]
enum Root<'a> {
    Entry(usize),
    Retry(u32, &'a [(ObjectId, EncodedObject)]),
    External(ObjectId, &'a EncodedObject),
}
impl<V, E> Engine<'_, '_, V, E>
where
    V: Fn(&ResolvedObject<'_>) -> std::result::Result<(), E> + Sync,
    E: Send,
{
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State<E>>> {
        self.state
            .lock()
            .map_err(|_| invalid("pack state poisoned"))
    }
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
    }
    // Every decode, replay included, spends the shared work budgets. Empty
    // DEFLATE blocks cost CPU without output, so compressed input is charged too.
    fn work(&self, compressed: u64, inflated: u64, decoded: u64) -> Result<()> {
        if self.limits.is_unlimited() {
            return Ok(());
        }
        self.check()?;
        let limit = self.limits.max_total_decoded;
        let [c, i, d] = &mut self.state()?.work;
        charge(c, compressed, limit.saturating_add(self.pack.len() as u64))?;
        charge(i, inflated, limit)?;
        charge(d, decoded, limit)
    }
    fn operation(&self) -> Result<()> {
        if self.limits.is_unlimited() {
            return Ok(());
        }
        if let Err(error) = resolve_op(self.metrics, self.limits.max_resolve_ops) {
            self.fail(error);
            return Err(GitError::Cancelled);
        }
        Ok(())
    }
    fn flight(&self) -> Result<Flight<'_>> {
        let bytes = self.workspace;
        if bytes > self.limits.max_in_flight_bytes {
            return Err(budget(
                ResourceLimitKind::ResolveInFlight,
                self.limits.max_in_flight_bytes as u64,
                bytes as u64,
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
    // Replay spends the same budgets; only profiling excludes it.
    fn inflate(&self, index: usize, _replay: bool) -> Result<Vec<u8>> {
        self.check()?;
        #[cfg(feature = "fetch-profile")]
        let _span = (!_replay).then(|| {
            sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::Inflate)
        });
        self.operation()?;
        let d = &self.entries[index];
        let size = size_check(d.header.size, self.limits)?;
        let decoded = if d.base.is_none() { d.header.size } else { 0 };
        self.work((d.end - d.data) as u64, d.header.size, decoded)?;
        parallel_index::inflate_exact(&self.pack[d.data..d.end], size, self.stop_flag())
            .map(|pair| pair.0)
    }
    fn apply(&self, index: usize, base: &[u8], delta: &[u8], _replay: bool) -> Result<Vec<u8>> {
        self.check()?;
        #[cfg(feature = "fetch-profile")]
        let _span = (!_replay).then(|| {
            sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::DeltaResolution)
        });
        self.operation()?;
        let plan = plan_pack_delta(base, delta)?;
        let size = size_check(plan.result_size, self.limits)?;
        self.work(0, 0, plan.result_size)?;
        let mut body = reserve(size)?;
        apply_pack_delta_exact(base, delta, plan, &mut body, self.stop_flag())?;
        if body.len() as u64 != self.entries[index].result_size {
            return Err(invalid("delta result changed"));
        }
        #[cfg(feature = "fetch-profile")]
        if !_replay {
            sley_core::fetch_profile::add_count(
                sley_core::fetch_profile::Stage::DeltaResolution,
                1,
            );
        }
        Ok(body)
    }
    fn retain<'a>(&'a self, node: &mut Node<'a>, body: Vec<u8>) -> Result<Base<'a>> {
        let size = body.len();
        if node.pending.load(AtomicOrdering::Relaxed) > 1
            && let Ok(used) = self.metrics.live.try_update(
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
                |used| {
                    used.checked_add(size)
                        .filter(|sum| *sum <= self.limits.max_live_base_bytes)
                },
            )
        {
            self.metrics
                .peak_live
                .fetch_max(used + size, AtomicOrdering::AcqRel);
            let retained = Arc::new(Retained {
                body,
                live: &self.metrics.live,
            });
            *node
                .body
                .get_mut()
                .map_err(|_| invalid("pack body poisoned"))? = Some(Arc::clone(&retained));
            Ok(Base::Retained(retained))
        } else {
            // The next DFS base belongs to this worker's existing workspace.
            Ok(Base::Owned(body))
        }
    }
    fn replay<'a>(&'a self, node: &Arc<Node<'a>>) -> Result<Base<'a>> {
        let mut chain = Vec::new();
        let mut current = Arc::clone(node);
        let mut body = loop {
            let retained = current
                .body
                .lock()
                .map_err(|_| invalid("pack body poisoned"))?
                .clone();
            if let Some(body) = retained {
                break Base::Retained(body);
            }
            if let Some(parent) = &current.parent {
                let parent = Arc::clone(parent);
                chain.push(current);
                current = parent;
            } else {
                break match current.index {
                    Some(index) => Base::Owned(self.inflate(index as usize, true)?),
                    None => Base::External(
                        current
                            .external
                            .ok_or_else(|| invalid("missing external base"))?,
                    ),
                };
            }
        };
        for node in chain.into_iter().rev() {
            let index = node.index.ok_or_else(|| invalid("missing replay entry"))? as usize;
            let delta = self.inflate(index, true)?;
            body = Base::Owned(self.apply(index, body.as_ref(), &delta, true)?);
        }
        Ok(body)
    }
    fn publish<'a>(
        &'a self,
        index: usize,
        body: &[u8],
        object_type: ObjectType,
        parent: Option<Arc<Node<'a>>>,
        delta: Option<ResolvedDelta<'_>>,
        completed: &AtomicUsize,
    ) -> std::result::Result<Node<'a>, ResolveError<E>> {
        self.check()?;
        let depth = parent.as_ref().map_or(0, |p| p.depth + 1);
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
            depth,
            delta,
        })
        .map_err(ResolveError::Visitor)?;
        self.check()?;
        let mut state = self.state()?;
        let mut children = std::mem::take(&mut state.children[index]);
        children.append(&mut state.take_refs(&oid));
        state.output[index] = Some((
            oid,
            object_type,
            crc32fast::hash(&self.pack[d.offset..d.end]),
            depth,
        ));
        state.prefer(&oid, index, depth);
        drop(state);
        completed.fetch_add(1, AtomicOrdering::Release);
        Ok(self.node(
            Some(index as u32),
            oid,
            object_type,
            depth,
            parent,
            children,
            None,
        ))
    }
    // A depth refusal waits for all possible shallower duplicates. Only indices
    // are deferred; the retry rebuilds the base along settled shortest edges.
    fn retry<'a>(
        &'a self,
        index: u32,
        external: &'a [(ObjectId, EncodedObject)],
    ) -> Result<Arc<Node<'a>>> {
        let state = self.state()?;
        let (mut chain, mut at, mut parent) = (Vec::new(), index, None);
        while let Some(base) = &self.entries[at as usize].base {
            at = match *base {
                DeltaBase::Offset(offset) => {
                    self.entries
                        .binary_search_by_key(&offset, |d| d.offset as u64)
                        .map_err(|_| invalid("missing resolved base"))? as u32
                }
                DeltaBase::Ref(oid) => match external.binary_search_by_key(&oid, |e| e.0) {
                    Ok(i) => {
                        let (ty, body) = (external[i].1.object_type, &external[i].1.body);
                        parent = Some(Arc::new(self.node(
                            None,
                            oid,
                            ty,
                            0,
                            None,
                            vec![],
                            Some(body),
                        )));
                        break;
                    }
                    Err(_) => state
                        .ref_slot(&oid)
                        .and_then(|i| state.refs[i].2)
                        .ok_or_else(|| invalid("missing settled REF base"))?,
                },
            };
            chain.push(at);
        }
        for i in chain.into_iter().rev() {
            let (oid, ty, _, depth) =
                state.output[i as usize].ok_or_else(|| invalid("unresolved REF candidate"))?;
            parent = Some(Arc::new(self.node(
                Some(i),
                oid,
                ty,
                depth,
                parent,
                vec![],
                None,
            )));
        }
        let mut parent = parent.ok_or_else(|| invalid("missing deferred base"))?;
        let node = Arc::get_mut(&mut parent).ok_or_else(|| invalid("shared deferred base"))?;
        *node
            .children
            .get_mut()
            .map_err(|_| invalid("pack children poisoned"))? = vec![index].into_iter();
        node.pending = AtomicU32::new(1);
        Ok(parent)
    }
    #[allow(clippy::too_many_arguments)]
    fn node<'a>(
        &self,
        index: Option<u32>,
        oid: ObjectId,
        object_type: ObjectType,
        depth: u16,
        parent: Option<Arc<Node<'a>>>,
        children: Vec<u32>,
        external: Option<&'a [u8]>,
    ) -> Node<'a> {
        Node {
            index,
            oid,
            object_type,
            offset: index.map_or(0, |i| self.entries[i as usize].offset as u64),
            depth,
            parent,
            pending: AtomicU32::new(children.len() as u32),
            children: Mutex::new(children.into_iter()),
            body: Mutex::new(None),
            external,
        }
    }
    fn root<'a>(
        &'a self,
        root: Root<'a>,
        completed: &AtomicUsize,
    ) -> std::result::Result<(Arc<Node<'a>>, Base<'a>), ResolveError<E>> {
        match root {
            Root::Entry(index) => {
                let body = self.inflate(index, false)?;
                let ty = parallel_index::object_type_for_kind(self.entries[index].header.kind)?;
                let mut node = self.publish(index, &body, ty, None, None, completed)?;
                let body = self.retain(&mut node, body)?;
                Ok((Arc::new(node), body))
            }
            Root::Retry(index, external) => {
                let node = self.retry(index, external)?;
                let body = self.replay(&node)?;
                Ok((node, body))
            }
            Root::External(oid, object) => {
                let children = self.state()?.take_refs(&oid);
                let (ty, body) = (object.object_type, &object.body);
                let node = self.node(None, oid, ty, 0, None, children, Some(body));
                Ok((Arc::new(node), Base::External(body)))
            }
        }
    }
    fn child<'a>(
        &'a self,
        parent: Arc<Node<'a>>,
        index: usize,
        base: Base<'a>,
        completed: &AtomicUsize,
    ) -> std::result::Result<Option<(Arc<Node<'a>>, Base<'a>)>, ResolveError<E>> {
        if parent.depth as usize >= self.limits.max_delta_depth.min(u16::MAX as usize) {
            self.state()?.deferred.push(index as u32);
            parent.release()?;
            return Ok(None);
        }
        let delta = self.inflate(index, false)?;
        let body = self.apply(index, base.as_ref(), &delta, false)?;
        drop(base);
        // Last direct apply releases the parent's body before the child visitor.
        parent.release()?;
        let info = ResolvedDelta {
            base: parent.oid,
            base_offset: parent.offset,
            instructions: &delta,
        };
        let ty = parent.object_type;
        let mut node = self.publish(index, &body, ty, Some(parent), Some(info), completed)?;
        let body = self.retain(&mut node, body)?;
        Ok(Some((Arc::new(node), body)))
    }
    // Shortest depth of an entry through its base; None while that base is unpublished.
    fn depth(
        &self,
        state: &State<E>,
        index: usize,
        external: &[(ObjectId, EncodedObject)],
    ) -> Option<u16> {
        let base = match self.entries[index].base {
            None => return Some(0),
            Some(DeltaBase::Offset(offset)) => self
                .entries
                .binary_search_by_key(&offset, |d| d.offset as u64)
                .ok()?,
            Some(DeltaBase::Ref(oid)) if external.binary_search_by_key(&oid, |e| e.0).is_ok() => {
                return Some(1);
            }
            Some(DeltaBase::Ref(oid)) => state.refs[state.ref_slot(&oid)?].2? as usize,
        };
        state.output[base].and_then(|o| o.3.checked_add(1))
    }
    fn settle<'a, P: FnMut(PackIndexProgress)>(
        &'a self,
        external: &'a [(ObjectId, EncodedObject)],
        threads: usize,
        progress: &mut P,
        count: &mut u64,
    ) -> Result<()> {
        let limit = self.limits.max_delta_depth.min(u16::MAX as usize);
        loop {
            let mut state = self.state()?;
            if state.error.is_some() || state.deferred.is_empty() {
                return Ok(());
            }
            // Relax published depths to shortest paths; depths only fall, so
            // this converges within max_delta_depth linear passes.
            let mut changed = true;
            while std::mem::take(&mut changed) {
                for i in 0..self.entries.len() {
                    if let Some((oid, ty, crc, old)) = state.output[i]
                        && let Some(depth) = self.depth(&state, i, external)
                        && depth < old
                    {
                        state.output[i] = Some((oid, ty, crc, depth));
                        state.prefer(&oid, i, depth);
                        changed = true;
                    }
                }
            }
            let mut roots = std::mem::take(&mut state.deferred);
            let (first, mut rejected) = (roots[0], reserve(self.entries.len())?);
            roots.retain(|&i| {
                let ready = self
                    .depth(&state, i as usize, external)
                    .is_some_and(|d| d as usize <= limit);
                if !ready {
                    rejected.push(i);
                }
                ready
            });
            state.deferred = rejected;
            if roots.is_empty() {
                let limits = PackReadLimits {
                    max_delta_depth: limit,
                    ..PackReadLimits::default()
                };
                let offset = self.entries[first as usize].offset;
                return parallel_index::check_delta_depth(offset, limit + 1, limits);
            }
            drop(state);
            self.run(&roots, true, external, threads, progress, count);
        }
    }
    fn run<'a, P: FnMut(PackIndexProgress)>(
        &'a self,
        roots: &[u32],
        settled: bool,
        external: &'a [(ObjectId, EncodedObject)],
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
        let root_count = roots.len() + if settled { 0 } else { external.len() };
        if root_count == 0 {
            return;
        }
        let workers = threads.max(1).min(self.entries.len()).min(slots);
        let queues: Vec<Mutex<VecDeque<Arc<Node<'a>>>>> =
            (0..workers).map(|_| Mutex::new(VecDeque::new())).collect();
        // Seed unclaimed roots too: no worker can observe zero between a root
        // claim and its publication. Enqueue increments before exposing a node.
        let outstanding = AtomicUsize::new(root_count);
        self.metrics.metadata.fetch_max(
            self.metadata_base
                + workers
                    * (self.limits.max_delta_depth.min(u16::MAX as usize) + 1)
                    * (std::mem::size_of::<Node<'_>>()
                        + std::mem::size_of::<Retained<'_>>()
                        + 32
                        + 6 * std::mem::size_of::<usize>()),
            AtomicOrdering::Relaxed,
        );
        let completed = AtomicUsize::new(0);
        let initial = *count;
        std::thread::scope(|scope| {
            let _coordinator = UnwindStop(|| self.fail(ResolveError::Panic));
            let mut handles = Vec::new();
            for worker in 0..workers {
                let completed = &completed;
                let cursor = &cursor;
                let queues = &queues;
                let outstanding = &outstanding;
                let handle = std::thread::Builder::new().spawn_scoped(
                    scope,
                    sley_core::diagnostics::inherit(move || {
                        // First local, last drop: includes buffers and all worker Arc clones.
                        let _flight;
                        let _unwind = UnwindStop(|| self.fail(ResolveError::Panic));
                        _flight = if self.limits.is_unlimited() {
                            None
                        } else {
                            match self.flight() {
                                Ok(lease) => Some(lease),
                                Err(error) => {
                                    self.fail(error.into());
                                    return;
                                }
                            }
                        };
                        let work = || -> std::result::Result<(), ResolveError<E>> {
                            let pop = |i: usize, steal: bool| -> Result<Option<Arc<Node<'a>>>> {
                                let mut queue = queues[i]
                                    .lock()
                                    .map_err(|_| invalid("pack queue poisoned"))?;
                                Ok(if steal {
                                    queue.pop_front()
                                } else {
                                    queue.pop_back()
                                })
                            };
                            let mut local = None;
                            while !self.stop.is_cancelled() {
                                self.check()?;
                                let mut item = local.take();
                                if item.is_none() {
                                    item = pop(worker, false)?.map(|node| (node, None));
                                }
                                if item.is_none() {
                                    if let Ok(i) = cursor.try_update(
                                        AtomicOrdering::Relaxed,
                                        AtomicOrdering::Relaxed,
                                        |i| if i < root_count { Some(i + 1) } else { None },
                                    ) {
                                        let root = if i < roots.len() {
                                            if settled {
                                                Root::Retry(roots[i], external)
                                            } else {
                                                Root::Entry(roots[i] as usize)
                                            }
                                        } else {
                                            let (oid, object) = &external[i - roots.len()];
                                            Root::External(*oid, object)
                                        };
                                        let (node, body) = self.root(root, completed)?;
                                        if node.pending.load(AtomicOrdering::Relaxed) != 0 {
                                            local = Some((node, Some(body)));
                                        } else {
                                            outstanding.fetch_sub(1, AtomicOrdering::AcqRel);
                                        }
                                        continue;
                                    }
                                    for other in 1..workers {
                                        item = pop((worker + other) % workers, true)?
                                            .map(|node| (node, None));
                                        if item.is_some() {
                                            break;
                                        }
                                    }
                                }
                                if let Some((node, body)) = item {
                                    let (index, more) = {
                                        let mut children = node
                                            .children
                                            .lock()
                                            .map_err(|_| invalid("pack children poisoned"))?;
                                        (
                                            children
                                                .next()
                                                .ok_or_else(|| invalid("empty pack work item"))?,
                                            children.len() != 0,
                                        )
                                    };
                                    if more {
                                        outstanding.fetch_add(1, AtomicOrdering::AcqRel);
                                        queues[worker]
                                            .lock()
                                            .map_err(|_| invalid("pack queue poisoned"))?
                                            .push_back(Arc::clone(&node));
                                    }
                                    let base = match body {
                                        Some(body) => body,
                                        None => self.replay(&node)?,
                                    };
                                    if let Some((child, body)) =
                                        self.child(node, index as usize, base, completed)?
                                        && child.pending.load(AtomicOrdering::Relaxed) != 0
                                    {
                                        local = Some((child, Some(body)));
                                    } else {
                                        outstanding.fetch_sub(1, AtomicOrdering::AcqRel);
                                    }
                                } else if outstanding.load(AtomicOrdering::Acquire) == 0 {
                                    break;
                                } else {
                                    std::thread::park_timeout(std::time::Duration::from_micros(50));
                                }
                            }
                            Ok(())
                        };
                        if let Err(error) = work() {
                            self.fail(error);
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
            loop {
                let finished = handles
                    .iter()
                    .all(std::thread::ScopedJoinHandle::is_finished);
                let next = initial + completed.load(AtomicOrdering::Acquire) as u64;
                // Coalescing must still expose an intermediate completed object
                // when a small pack finishes between coordinator polls.
                if *count == initial && next > initial + 1 {
                    progress(PackIndexProgress {
                        completed_objects: initial + 1,
                        total_objects: self.entries.len() as u64,
                    });
                }
                *count = next;
                progress(PackIndexProgress {
                    completed_objects: *count,
                    total_objects: self.entries.len() as u64,
                });
                if finished {
                    break;
                }
                if let Err(error) = self.cancel.check() {
                    self.fail(error.into());
                }
                std::thread::park_timeout(std::time::Duration::from_millis(10));
            }
            for handle in handles {
                if handle.join().is_err() {
                    self.fail(ResolveError::Panic);
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
    let (checksum, entries) = discover(pack, format, limits, cancel, metrics, threads)?;
    let overflow = || {
        budget(
            ResourceLimitKind::ResolveInFlight,
            limits.max_in_flight_bytes as u64,
            u64::MAX,
        )
    };
    let max_body = entries.iter().map(|d| d.result_size).max().unwrap_or(0);
    let mut workspace = max_body;
    for d in entries.iter().filter(|d| d.base.is_some()) {
        workspace = workspace.max(
            max_body
                .checked_add(d.result_size)
                .and_then(|size| size.checked_add(d.header.size))
                .ok_or_else(overflow)?,
        );
    }
    let workspace = usize::try_from(workspace).map_err(|_| overflow())?;
    let mut children: Vec<Vec<u32>> = reserve(entries.len())?;
    children.resize_with(entries.len(), Vec::new);
    let mut refs = reserve(
        entries
            .iter()
            .filter(|d| matches!(d.base, Some(DeltaBase::Ref(_))))
            .count(),
    )?;
    let mut roots = Vec::new();
    for (index, d) in entries.iter().enumerate() {
        match d.base {
            None => roots.push(index as u32),
            Some(DeltaBase::Ref(oid)) => refs.push((oid, index as u32)),
            Some(DeltaBase::Offset(offset)) => {
                let base = entries
                    .binary_search_by_key(&offset, |d| d.offset as u64)
                    .map_err(|_| invalid("ofs-delta base offset not found"))?;
                if base >= index {
                    return Err(invalid("pack delta cycle detected").into());
                }
                children[base].push(index as u32);
            }
        }
    }
    // Sorted, stable slots avoid opaque tree-node overhead and quadratic removal.
    refs.sort_unstable_by_key(|entry| entry.0);
    let ref_input = allocation(&refs);
    let mut grouped: Vec<(ObjectId, Vec<u32>, Option<u32>)> = reserve(refs.len())?;
    for (oid, index) in refs {
        if let Some((last, indices, _)) = grouped.last_mut()
            && *last == oid
        {
            indices.push(index);
        } else {
            grouped.push((oid, vec![index], None));
        }
    }
    let refs = grouped;
    let links = children.iter().map(allocation).sum::<usize>()
        + refs.iter().map(|r| allocation(&r.1)).sum::<usize>();
    let discovery_metadata = allocation(&entries)
        + allocation(&roots)
        + allocation(&children)
        + allocation(&refs)
        + 2 * links;
    metrics
        .metadata
        .fetch_max(discovery_metadata + ref_input, AtomicOrdering::Relaxed);
    let mut output = reserve(entries.len())?;
    output.resize_with(entries.len(), || None);
    let buffers = allocation(&entries) + allocation(&output);
    let metadata =
        discovery_metadata + allocation(&output) + 2 * entries.len() * std::mem::size_of::<u32>();
    metrics.metadata.fetch_max(
        metadata.max(
            buffers
                + entries.len()
                    * (std::mem::size_of::<PackIndexEntry>()
                        + std::mem::size_of::<PackIndexedObject>()),
        ),
        AtomicOrdering::Relaxed,
    );
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
            work: [0; 3],
            deferred: reserve(entries.len())?,
        }),
        metrics,
        workspace,
        metadata_base: metadata,
        flight_wait: Mutex::new(()),
        flight_wake: Condvar::new(),
    };
    let mut count = 0;
    if entries.is_empty() {
        progress(PackIndexProgress::default());
    }
    engine.run(&roots, false, &[], threads, progress, &mut count);
    // The pending REF map only shrinks. Snapshot once: repeatedly searching
    // past missing bases would make a thin pack with distinct bases quadratic.
    let pending: Vec<_> = engine
        .state()?
        .refs
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.1.is_empty())
        .map(|(i, _)| i as u32)
        .collect();
    metrics
        .metadata
        .fetch_max(metadata + allocation(&pending), AtomicOrdering::Relaxed);
    if let Some(error) = engine.state()?.error.take() {
        return Err(error);
    }
    let mut externals = Vec::new();
    for slot in pending {
        cancel.check()?;
        let oid = engine.state()?.refs[slot as usize].0;
        if let Some(object) = external(&oid)? {
            size_check(object.body.len() as u64, limits)?;
            if object.object_id(format)? != oid {
                return Err(invalid("external delta base identity mismatch").into());
            }
            externals.push((oid, object));
        }
    }
    engine.run(&[], false, &externals, threads, progress, &mut count);
    engine.settle(&externals, threads, progress, &mut count)?;
    {
        let mut state = engine.state()?;
        if let Some(error) = state.error.take() {
            return Err(error);
        }
    }
    if count != entries.len() as u64 {
        return Err(invalid("unresolved, cyclic, or external delta base").into());
    }
    cancel.check()?;
    drop(roots);
    let State { output, .. } = engine
        .state
        .into_inner()
        .map_err(|_| invalid("pack state poisoned"))?;
    let mut indexed = reserve(entries.len())?;
    let mut objects = reserve(entries.len())?;
    for (d, entry) in entries.iter().zip(output) {
        cancel.check()?;
        let (oid, object_type, crc32, _) = entry.ok_or_else(|| invalid("unresolved pack entry"))?;
        indexed.push(PackIndexEntry {
            oid,
            offset: d.offset as u64,
            crc32,
        });
        objects.push(PackIndexedObject {
            oid,
            offset: d.offset as u64,
            object_type,
            size: d.result_size,
        });
    }
    drop(entries);
    let index = PackIndex::write_v2(format, &indexed, &checksum)?;
    metrics.metadata.fetch_max(
        allocation(&indexed)
            + allocation(&objects)
            + allocation(&index)
            + indexed.len()
                * (std::mem::size_of::<&PackIndexEntry>() + 2 * std::mem::size_of::<u64>()),
        AtomicOrdering::Relaxed,
    );
    cancel.check()?;
    Ok(PackIndexBuild {
        index,
        pack_checksum: checksum,
        entries: indexed,
        objects,
    })
}
