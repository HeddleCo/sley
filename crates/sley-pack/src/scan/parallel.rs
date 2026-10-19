//! Dependency-ready batches over the scan's authoritative grammar and accounting.
use super::*;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A logical materialization cap shared by concurrent scans and remote reads.
/// Caller-retained outputs, index metadata, allocator slack and I/O scratch are
/// excluded. Share one `Arc` across readers to impose a process-wide ceiling.
#[derive(Debug)]
pub struct PackDecodeBudget {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
    #[cfg(feature = "async-budget")]
    available: Arc<tokio::sync::Semaphore>,
}

impl PackDecodeBudget {
    /// With `async-budget`, byte permits cap the budget at `u32::MAX` bytes
    /// (or the platform semaphore maximum, if smaller).
    pub fn new(limit: usize) -> Self {
        #[cfg(feature = "async-budget")]
        let limit = limit
            .min(u32::MAX as usize)
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        Self {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            #[cfg(feature = "async-budget")]
            available: Arc::new(tokio::sync::Semaphore::new(limit)),
        }
    }
    pub const fn limit_bytes(&self) -> usize {
        self.limit
    }
    pub fn used_bytes(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    pub fn peak_bytes(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> ScanResult<PackDecodeReservation> {
        let mut reservation = PackDecodeReservation {
            budget: Arc::clone(self),
            bytes: 0,
            #[cfg(feature = "async-budget")]
            permit: None,
        };
        reservation.resize(bytes)?;
        Ok(reservation)
    }

    /// Wait for a whole reservation, without retaining a partial allocation.
    #[cfg(feature = "async-budget")]
    pub async fn reserve_async(
        self: &Arc<Self>,
        bytes: usize,
    ) -> ScanResult<PackDecodeReservation> {
        check_limit(PackLimitKind::MaterializedBytes, self.limit, bytes)?;
        let permit = Arc::clone(&self.available)
            .acquire_many_owned(bytes as u32)
            .await
            .map_err(|_| GitError::Cancelled)?;
        let previous = self.used.fetch_add(bytes, Ordering::Relaxed);
        self.peak.fetch_max(previous + bytes, Ordering::Relaxed);
        Ok(PackDecodeReservation {
            budget: Arc::clone(self),
            bytes,
            permit: Some(permit),
        })
    }
}

/// A reservation releases its charge even on error, cancellation or drop.
pub struct PackDecodeReservation {
    budget: Arc<PackDecodeBudget>,
    bytes: usize,
    #[cfg(feature = "async-budget")]
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl PackDecodeReservation {
    pub fn resize(&mut self, bytes: usize) -> ScanResult<()> {
        if bytes > self.bytes {
            let additional = bytes - self.bytes;
            #[cfg(feature = "async-budget")]
            let acquired = {
                check_limit(PackLimitKind::MaterializedBytes, self.budget.limit, bytes)?;
                Arc::clone(&self.budget.available)
                    .try_acquire_many_owned(additional as u32)
                    .map_err(|_| {
                        PackReadError::Limit(PackLimitError {
                            kind: PackLimitKind::MaterializedBytes,
                            limit: self.budget.limit,
                            attempted: self.budget.used_bytes().saturating_add(additional),
                        })
                    })?
            };
            let previous = self
                .budget
                .used
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                    used.checked_add(additional)
                        .filter(|next| *next <= self.budget.limit)
                })
                .map_err(|used| {
                    PackReadError::Limit(PackLimitError {
                        kind: PackLimitKind::MaterializedBytes,
                        limit: self.budget.limit,
                        attempted: used.saturating_add(additional),
                    })
                })?;
            #[cfg(feature = "async-budget")]
            if let Some(permit) = &mut self.permit {
                permit.merge(acquired);
            } else {
                self.permit = Some(acquired);
            }
            self.budget
                .peak
                .fetch_max(previous + additional, Ordering::Relaxed);
        } else {
            self.budget
                .used
                .fetch_sub(self.bytes - bytes, Ordering::Relaxed);
            #[cfg(feature = "async-budget")]
            if let Some(permit) = &mut self.permit {
                drop(permit.split(self.bytes - bytes));
            }
        }
        self.bytes = bytes;
        Ok(())
    }
}
impl Drop for PackDecodeReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Bounds CPU concurrency and the number of entries in each work batch.
#[derive(Debug, Clone)]
pub struct ParallelScanOptions {
    pub threads: usize,
    pub window: usize,
    /// Reuse an executor across cursors. Its thread count controls CPU concurrency.
    pub pool: Option<Arc<rayon::ThreadPool>>,
}
impl ParallelScanOptions {
    pub const fn new(threads: usize, window: usize) -> Self {
        Self {
            threads,
            window,
            pool: None,
        }
    }
}
impl Default for ParallelScanOptions {
    fn default() -> Self {
        Self::new(4, 1024)
    }
}

impl<'a, S: PackReadSource + Sync> PackScanPlan<'a, S> {
    /// Decode ready dependencies on CPU threads, yielding in pack order.
    ///
    /// At most `window` output positions are admitted at once, plus their
    /// transitive bases (including forward REF bases). Each batch has at most
    /// `window` jobs; all retained bodies and instructions obey the scan limits
    /// and the shared budget. Batches shrink to fit available budget; a single
    /// job that cannot fit fails closed. No entry is inflated twice. Dropping the cursor cancels further work; each `next`
    /// waits for its bounded batch before returning. External lookup runs on the
    /// caller, outside workers and without locks. Use a blocking CPU executor
    /// when embedding this synchronous iterator in an async application.
    pub fn parallel_cursor<F>(
        self,
        external: F,
        cancel: CancelFlag<'a>,
        options: ParallelScanOptions,
        budget: Arc<PackDecodeBudget>,
    ) -> ParallelPackScanCursor<'a, S, F>
    where
        F: FnMut(&ObjectId) -> ScanResult<Arc<EncodedObject>>,
    {
        let count = self.scan.entries.len();
        ParallelPackScanCursor {
            inner: self.cursor(external, cancel),
            workers: options.pool.clone(),
            options,
            reservation: PackDecodeReservation {
                budget,
                bytes: 0,
                #[cfg(feature = "async-budget")]
                permit: None,
            },
            admitted: Vec::new(),
            queued: vec![false; count],
            peak_in_flight: 0,
            inflated: std::collections::VecDeque::new(),
        }
    }
}

/// Bounded streaming parallel decoder; errors are terminal, as for the cursor.
pub struct ParallelPackScanCursor<'a, S, F> {
    inner: PackScanCursor<'a, S, F>,
    options: ParallelScanOptions,
    reservation: PackDecodeReservation,
    admitted: Vec<usize>,
    queued: Vec<bool>,
    peak_in_flight: usize,
    workers: Option<Arc<rayon::ThreadPool>>,
    inflated: std::collections::VecDeque<(usize, Vec<u8>)>,
}

impl<S: PackReadSource + Sync, F: FnMut(&ObjectId) -> ScanResult<Arc<EncodedObject>>>
    ParallelPackScanCursor<'_, S, F>
{
    pub const fn stats(&self) -> PackScanStats {
        self.inner.stats()
    }
    pub const fn peak_in_flight(&self) -> usize {
        self.peak_in_flight
    }

    fn allocate(&mut self, size: u64) -> ScanResult<Vec<u8>> {
        let size = usize::try_from(size)
            .map_err(|_| GitError::InvalidObject("entry size exceeds usize".into()))?;
        let total = self.inner.active_bytes.saturating_add(size);
        check_limit(
            PackLimitKind::MaterializedBytes,
            self.inner.plan.scan.limits.max_materialized_bytes,
            total,
        )?;
        self.reservation.resize(total)?;
        let bytes = self.inner.allocate(size as u64)?;
        self.inner.active_bytes = total;
        Ok(bytes)
    }

    fn refill(&mut self) -> ScanResult<()> {
        let window = self.options.window;
        if window == 0 || self.options.threads == 0 {
            return Err(GitError::InvalidFormat(
                "parallel scan threads and window must be nonzero".into(),
            )
            .into());
        }
        if self.options.threads > 1 && self.workers.is_none() {
            self.workers = Some(Arc::new(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(self.options.threads)
                    .stack_size(2 * 1024 * 1024)
                    .build()
                    .map_err(|error| {
                        GitError::InvalidObject(format!(
                            "parallel scan worker startup failed: {error}"
                        ))
                    })?,
            ));
        }
        let end = self
            .inner
            .output
            .saturating_add(window)
            .min(self.inner.plan.planned.len());
        for index in self.inner.output..end {
            let mut position = self.inner.plan.planned[index];
            while !self.queued[position] {
                self.queued[position] = true;
                self.admitted.push(position);
                match self.inner.plan.scan.entries[position].base_position {
                    Some(base) => position = base,
                    None => break,
                }
            }
        }
        let target = self.inner.plan.planned[self.inner.output];
        while self.inner.states[target].object.is_none() {
            self.inner.cancel.check()?;
            let scan = self.inner.plan.scan;
            let cancel = self.inner.cancel;
            if self.inflated.is_empty() {
                let mut ready = Vec::new();
                for &position in &self.admitted {
                    let entry = &self.inner.plan.scan.entries[position];
                    if entry.base_position.is_none() || self.inner.base(position).is_some() {
                        ready.push(position);
                        if ready.len() == window {
                            break;
                        }
                    }
                }
                if ready.is_empty() {
                    return Err(
                        GitError::InvalidObject("planned entry has no ready base".into()).into(),
                    );
                }
                let mut jobs = Vec::with_capacity(ready.len());
                for &position in &ready {
                    self.inner.load_external(position)?;
                    self.reservation.resize(self.inner.active_bytes)?;
                    let size = self.inner.plan.scan.entries[position].public.declared_size;
                    match self.allocate(size) {
                        Ok(body) => jobs.push((position, body)),
                        Err(PackReadError::Limit(_)) if !jobs.is_empty() => break,
                        Err(error) => return Err(error),
                    }
                }
                self.peak_in_flight = self.peak_in_flight.max(jobs.len());
                let inflated = run_jobs(jobs, self.workers.as_deref(), |(position, mut body)| {
                    let entry = &scan.entries[position];
                    let expected = usize::try_from(entry.public.declared_size)
                        .map_err(|_| GitError::InvalidObject("entry size exceeds usize".into()))?;
                    let (consumed, _) = inflate_source_exact_into(
                        &scan.source,
                        entry.data_offset,
                        entry.end,
                        &mut body,
                        expected,
                        cancel,
                    )?;
                    if consumed != entry.end - entry.data_offset {
                        return Err(GitError::InvalidObject(
                            "compressed entry length differs from index span".into(),
                        )
                        .into());
                    }
                    Ok((position, body))
                })?;
                for (_, body) in &inflated {
                    self.inner.stats.entries_inflated += 1;
                    self.inner.stats.bytes_inflated += body.len() as u64;
                }
                self.inflated.extend(inflated);
            }
            let mut jobs = Vec::with_capacity(self.inflated.len());
            while let Some((position, body)) = self.inflated.pop_front() {
                let base = self.inner.base(position);
                let result = if let Some(base) = &base {
                    let plan = plan_pack_delta(&base.body, &body)?;
                    match self.allocate(plan.result_size) {
                        Ok(output) => Some((plan, output)),
                        Err(PackReadError::Limit(_)) if !jobs.is_empty() => {
                            // Keep already-inflated instructions charged and resume
                            // after this smaller batch releases its consumed bases.
                            self.inflated.push_front((position, body));
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                };
                jobs.push((position, body, base, result));
            }
            let objects = run_jobs(
                jobs,
                self.workers.as_deref(),
                |(position, body, base, result)| {
                    cancel.check()?;
                    let (object, delta_bytes) = match (base, result) {
                        (Some(base), Some((plan, mut output))) => {
                            apply_pack_delta_exact(&base.body, &body, plan, &mut output, cancel)?;
                            (EncodedObject::new(base.object_type, output), body.len())
                        }
                        _ => match scan.entries[position].public.kind {
                            PackScanKind::Object(kind) => (EncodedObject::new(kind, body), 0),
                            _ => {
                                return Err(GitError::InvalidObject(
                                    "missing ready delta base".into(),
                                )
                                .into());
                            }
                        },
                    };
                    let oid = crate::parallel_index::cancellable_object_id_bytes(
                        object.object_type,
                        &object.body,
                        scan.format,
                        cancel,
                    )?;
                    if oid != scan.entries[position].public.oid {
                        return Err(GitError::InvalidObject(
                            "scanned object identity differs from index".into(),
                        )
                        .into());
                    }
                    Ok((position, Arc::new(object), delta_bytes))
                },
            )?;
            for (position, object, delta_bytes) in objects {
                self.inner.active_bytes = subtract(self.inner.active_bytes, delta_bytes)?;
                self.inner.consume_base(position)?;
                if self.inner.remaining[position] > 0 {
                    self.inner.live_bytes = self.inner.live_bytes.saturating_add(object.body.len());
                    check_limit(
                        PackLimitKind::LiveBaseBytes,
                        scan.limits.max_live_base_bytes,
                        self.inner.live_bytes,
                    )?;
                    self.inner.stats.peak_live_base_bytes = self
                        .inner
                        .stats
                        .peak_live_base_bytes
                        .max(self.inner.live_bytes);
                }
                self.inner.states[position].object = Some(object);
            }
            self.admitted
                .retain(|position| self.inner.states[*position].object.is_none());
            self.reservation.resize(self.inner.active_bytes)?;
        }
        Ok(())
    }
}

impl<S: PackReadSource + Sync, F: FnMut(&ObjectId) -> ScanResult<Arc<EncodedObject>>> Iterator
    for ParallelPackScanCursor<'_, S, F>
{
    type Item = ScanResult<PackScanObject>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.inner.failed || self.inner.output == self.inner.plan.planned.len() {
            return None;
        }
        let result = self
            .inner
            .cancel
            .check()
            .map_err(PackReadError::from)
            .and_then(|()| {
                let position = self.inner.plan.planned[self.inner.output];
                if self.inner.states[position].object.is_none() {
                    self.refill()?;
                }
                self.inner.next().ok_or_else(|| {
                    PackReadError::from(GitError::InvalidObject("missing parallel output".into()))
                })?
            });
        if result.is_err() {
            self.inner.failed = true;
            self.inflated.clear();
            self.inner.states.clear();
            self.inner.external.clear();
            self.inner.active_bytes = 0;
        }
        if let Err(error) = self.reservation.resize(self.inner.active_bytes) {
            return Some(Err(error));
        }
        Some(result)
    }
}
impl<S: PackReadSource + Sync, F: FnMut(&ObjectId) -> ScanResult<Arc<EncodedObject>>>
    std::iter::FusedIterator for ParallelPackScanCursor<'_, S, F>
{
}

// Owned heap jobs keep worker stacks independent of object size and depth.
// A supplied pool is shared across cursors; otherwise a cursor builds one pool.
fn run_jobs<T: Send, U: Send>(
    jobs: Vec<T>,
    pool: Option<&rayon::ThreadPool>,
    work: impl Fn(T) -> ScanResult<U> + Sync,
) -> ScanResult<Vec<U>> {
    if let Some(pool) = pool.filter(|_| jobs.len() > 1) {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| jobs.into_par_iter().map(&work).collect())
        }))
        .map_err(|_| GitError::InvalidObject("parallel scan worker panicked".into()))?
    } else {
        jobs.into_iter().map(work).collect()
    }
}
