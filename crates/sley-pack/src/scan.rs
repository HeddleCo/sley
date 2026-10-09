//! Plan-driven, iterative decoding in pack order.

use super::*;
use crate::bounded_read::{inflate_source_exact_into, object_type_for_entry};
use std::io;

type ScanResult<T> = std::result::Result<T, PackReadError>;

/// The kind stored in an entry header (delta result types are learned later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackScanKind {
    Object(ObjectType),
    OfsDelta,
    RefDelta,
}

/// An immediate delta base, classified using the accompanying index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackScanBase {
    InPack(ObjectId),
    External(ObjectId),
}

/// Header-only metadata, in pack offset order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackScanEntry {
    pub oid: ObjectId,
    pub offset: u64,
    pub kind: PackScanKind,
    /// Inflated stream size; for deltas this describes instructions, not output.
    pub declared_size: u64,
    pub base: Option<PackScanBase>,
}

struct ScanEntry {
    public: PackScanEntry,
    data_offset: u64,
    end: u64,
    base_position: Option<usize>,
}

/// A pack and its index, inspected without inflating any entry.
///
/// Index offsets delimit compressed streams, so construction reads only the
/// pack header, trailer and entry prefixes. The trailer must match the index;
/// this does not recompute the whole-pack checksum or inspect unplanned bodies.
/// Each planned body is checked for exact stream length and object identity.
/// The source must remain unchanged throughout the scan.
pub struct PackScan<S> {
    source: S,
    format: ObjectFormat,
    limits: PackReadLimits,
    entries: Vec<ScanEntry>,
    by_oid: HashMap<ObjectId, usize>,
}

impl<'a> PackScan<SlicePackSource<'a>> {
    /// Borrow a pack slice without copying it.
    pub fn from_slice(
        bytes: &'a [u8],
        index: &PackIndex,
        limits: PackReadLimits,
    ) -> ScanResult<Self> {
        Self::new(SlicePackSource::new(bytes), index, limits)
    }
}

impl<S: PackReadSource> PackScan<S> {
    /// Inspect a positional source; format is taken from the index checksum.
    pub fn new(source: S, index: &PackIndex, limits: PackReadLimits) -> ScanResult<Self> {
        let format = index.pack_checksum.format();
        let end = source
            .len()?
            .checked_sub(format.raw_len() as u64)
            .filter(|end| *end >= 12)
            .ok_or_else(|| {
                GitError::InvalidFormat("pack smaller than its header and trailer".into())
            })?;
        let mut header = [0u8; 12];
        read_exact(&source, 0, &mut header)?;
        if &header[..4] != b"PACK" || !matches!(u32_be(&header[4..8]), 2 | 3) {
            return Err(GitError::InvalidFormat("invalid pack header or version".into()).into());
        }
        let count = checked_pack_object_count(u32_be(&header[8..12]), end - 12)?;
        if count != index.entries.len() {
            return Err(GitError::InvalidFormat("pack/index entry count mismatch".into()).into());
        }
        let mut trailer = [0u8; 32];
        read_exact(&source, end, &mut trailer[..format.raw_len()])?;
        if &trailer[..format.raw_len()] != index.pack_checksum.as_bytes() {
            return Err(GitError::InvalidFormat("pack/index checksum mismatch".into()).into());
        }
        let mut ordered: Vec<_> = index.entries.iter().collect();
        ordered.sort_unstable_by_key(|entry| entry.offset);
        if ordered.first().map(|entry| entry.offset).unwrap_or(end) != 12 {
            return Err(GitError::InvalidFormat(
                "pack entries do not start after the header".into(),
            )
            .into());
        }
        let mut by_oid = HashMap::with_capacity(count);
        let mut by_offset = HashMap::with_capacity(count);
        for (position, entry) in ordered.iter().enumerate() {
            if entry.oid.format() != format
                || entry.offset < 12
                || entry.offset >= end
                || by_oid.insert(entry.oid, position).is_some()
                || by_offset.insert(entry.offset, position).is_some()
            {
                return Err(GitError::InvalidFormat(
                    "invalid or duplicate pack index entry".into(),
                )
                .into());
            }
        }
        let mut entries = Vec::with_capacity(count);
        for (position, entry) in ordered.iter().enumerate() {
            let entry_end = ordered
                .get(position + 1)
                .map(|next| next.offset)
                .unwrap_or(end);
            let available = (entry_end - entry.offset).min(64) as usize;
            let mut prefix = [0u8; 64];
            read_exact(&source, entry.offset, &mut prefix[..available])?;
            let bytes = &prefix[..available];
            let mut cursor = 0;
            let header = parse_entry_header(bytes, &mut cursor)?;
            let (kind, base, base_position) = match header.kind {
                PackObjectKind::OfsDelta => {
                    let offset = parse_ofs_delta_base_offset(bytes, &mut cursor, entry.offset)?;
                    let base = *by_offset
                        .get(&offset)
                        .filter(|base| **base < position)
                        .ok_or_else(|| {
                            GitError::InvalidFormat("ofs-delta base is not an earlier entry".into())
                        })?;
                    (
                        PackScanKind::OfsDelta,
                        Some(PackScanBase::InPack(ordered[base].oid)),
                        Some(base),
                    )
                }
                PackObjectKind::RefDelta => {
                    let raw_end = cursor + format.raw_len();
                    let raw = bytes.get(cursor..raw_end).ok_or_else(|| {
                        GitError::InvalidFormat("truncated ref-delta base object id".into())
                    })?;
                    let oid = ObjectId::from_raw(format, raw)?;
                    cursor = raw_end;
                    match by_oid.get(&oid).copied() {
                        Some(base) => (
                            PackScanKind::RefDelta,
                            Some(PackScanBase::InPack(oid)),
                            Some(base),
                        ),
                        None => (
                            PackScanKind::RefDelta,
                            Some(PackScanBase::External(oid)),
                            None,
                        ),
                    }
                }
                kind => (
                    PackScanKind::Object(object_type_for_entry(kind)?),
                    None,
                    None,
                ),
            };
            let data_offset = entry.offset + cursor as u64;
            if data_offset >= entry_end {
                return Err(
                    GitError::InvalidFormat("missing compressed pack entry body".into()).into(),
                );
            }
            entries.push(ScanEntry {
                public: PackScanEntry {
                    oid: entry.oid,
                    offset: entry.offset,
                    kind,
                    declared_size: header.size,
                    base,
                },
                data_offset,
                end: entry_end,
                base_position,
            });
        }
        Ok(Self {
            source,
            format,
            limits,
            entries,
            by_oid,
        })
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = &PackScanEntry> {
        self.entries.iter().map(|entry| &entry.public)
    }

    /// Add transitive in-pack bases, count direct dependents, and detect cycles.
    /// Duplicate requested IDs are counted once. An absent target is an error.
    pub fn plan(
        &self,
        needed: impl IntoIterator<Item = ObjectId>,
    ) -> ScanResult<PackScanPlan<'_, S>> {
        let count = self.entries.len();
        let mut selected = vec![false; count];
        let mut requested = vec![false; count];
        for oid in needed {
            let mut position = *self
                .by_oid
                .get(&oid)
                .ok_or_else(|| GitError::object_not_found(oid))?;
            requested[position] = true;
            while !selected[position] {
                selected[position] = true;
                match self.entries[position].base_position {
                    Some(base) => position = base,
                    None => break,
                }
            }
        }
        let planned: Vec<_> = (0..count).filter(|position| selected[*position]).collect();
        let mut dependents = vec![Vec::new(); count];
        let mut external = BTreeMap::new();
        let mut ready = VecDeque::new();
        for &position in &planned {
            let entry = &self.entries[position];
            if let Some(base) = entry.base_position {
                dependents[base].push(position);
            } else {
                ready.push_back(position);
                if let Some(PackScanBase::External(oid)) = entry.public.base {
                    *external.entry(oid).or_insert(0usize) += 1;
                }
            }
        }
        let mut depths = vec![0usize; count];
        let mut resolved = 0;
        while let Some(position) = ready.pop_front() {
            if self.entries[position].public.base.is_some()
                && self.entries[position].base_position.is_none()
            {
                depths[position] = 1;
            }
            check_limit(
                PackLimitKind::DeltaDepth,
                self.limits.max_delta_depth,
                depths[position],
            )?;
            resolved += 1;
            for &child in &dependents[position] {
                depths[child] = depths[position].saturating_add(1);
                ready.push_back(child);
            }
        }
        if resolved != planned.len() {
            return Err(GitError::InvalidObject("cycle in planned delta bases".into()).into());
        }
        Ok(PackScanPlan {
            scan: self,
            planned,
            requested,
            dependents,
            external,
        })
    }
}

/// Dependency closure tied to the scan from which it was built.
pub struct PackScanPlan<'a, S> {
    scan: &'a PackScan<S>,
    planned: Vec<usize>,
    requested: Vec<bool>,
    dependents: Vec<Vec<usize>>,
    external: BTreeMap<ObjectId, usize>,
}

impl<'a, S: PackReadSource> PackScanPlan<'a, S> {
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &PackScanEntry> {
        self.planned
            .iter()
            .map(|position| &self.scan.entries[*position].public)
    }

    /// Distinct external bases the caller must supply before iteration.
    pub fn external_bases(&self) -> impl ExactSizeIterator<Item = &ObjectId> {
        self.external.keys()
    }

    /// Supply resolved external objects by ID. Missing IDs produce
    /// `PackReadError::Pack(GitError::NotFound(_))`; identities are verified.
    /// Unneeded supplied objects are dropped before decoding.
    pub fn cursor(
        self,
        mut external: HashMap<ObjectId, EncodedObject>,
    ) -> ScanResult<PackScanCursor<'a, S>> {
        let mut bases = HashMap::new();
        let mut live_bytes = 0usize;
        for (oid, dependents) in &self.external {
            let object = external
                .remove(oid)
                .ok_or_else(|| GitError::object_not_found(*oid))?;
            let size = object.body.len();
            live_bytes = live_bytes.saturating_add(size);
            check_limit(
                PackLimitKind::LiveBaseBytes,
                self.scan.limits.max_cached_bytes,
                live_bytes,
            )?;
            check_limit(
                PackLimitKind::MaterializedBytes,
                self.scan.limits.max_materialized_bytes,
                live_bytes,
            )?;
            if object.object_id(self.scan.format)? != *oid {
                return Err(GitError::InvalidObject(
                    "external delta base identity mismatch".into(),
                )
                .into());
            }
            bases.insert(*oid, (Arc::new(object), *dependents));
        }
        let states = (0..self.scan.entries.len())
            .map(|_| ScanState::default())
            .collect();
        let remaining = self.dependents.iter().map(Vec::len).collect();
        Ok(PackScanCursor {
            plan: self,
            states,
            remaining,
            external: bases,
            input: 0,
            output: 0,
            active_bytes: live_bytes,
            live_bytes,
            failed: false,
            stats: PackScanStats {
                peak_live_base_bytes: live_bytes,
                ..PackScanStats::default()
            },
        })
    }
}

/// Cumulative work performed by a cursor, excluding supplied external bases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackScanStats {
    pub entries_inflated: u64,
    /// Inflated stream bytes (delta instructions rather than delta results).
    pub bytes_inflated: u64,
    /// Maximum logical resolved body bytes with outstanding dependents.
    /// Includes external bases, excludes caller-held outputs and pending deltas.
    pub peak_live_base_bytes: usize,
}

/// A resolved planned entry, including automatically added bases.
#[derive(Debug, Clone)]
pub struct PackScanObject {
    pub oid: ObjectId,
    pub offset: u64,
    /// Whether this ID was requested, rather than only added as a base.
    pub requested: bool,
    /// Shared with the cursor only while this object is a live base.
    pub object: Arc<EncodedObject>,
}

#[derive(Default)]
struct ScanState {
    delta: Option<Vec<u8>>,
    object: Option<Arc<EncodedObject>>,
    yielded: bool,
}

/// Inflates each planned entry once in pack order and yields in that order.
///
/// No dependency traversal is recursive. Forward REF bases require retaining
/// pending instructions and outputs until the next output can resolve.
/// `max_materialized_bytes` caps all active logical bodies/instructions before
/// allocation, including a delta's base, instructions and result together.
/// `max_cached_bytes` caps resolved live bases, with no re-inflation fallback.
/// Returned objects retained by the caller, allocator slack, metadata and fixed
/// I/O scratch are outside these bounds. Errors terminate iteration.
pub struct PackScanCursor<'a, S> {
    plan: PackScanPlan<'a, S>,
    states: Vec<ScanState>,
    remaining: Vec<usize>,
    external: HashMap<ObjectId, (Arc<EncodedObject>, usize)>,
    input: usize,
    output: usize,
    active_bytes: usize,
    live_bytes: usize,
    stats: PackScanStats,
    failed: bool,
}

impl<S: PackReadSource> PackScanCursor<'_, S> {
    pub const fn stats(&self) -> PackScanStats {
        self.stats
    }

    fn allocate(&self, requested: u64) -> ScanResult<Vec<u8>> {
        let size = usize::try_from(requested).map_err(|_| {
            PackReadError::Limit(PackLimitError {
                kind: PackLimitKind::MaterializedBytes,
                limit: self.plan.scan.limits.max_materialized_bytes,
                attempted: usize::MAX,
            })
        })?;
        check_limit(
            PackLimitKind::MaterializedBytes,
            self.plan.scan.limits.max_materialized_bytes,
            self.active_bytes.saturating_add(size),
        )?;
        let mut body = Vec::new();
        body.try_reserve_exact(size).map_err(|_| {
            PackReadError::Allocation(PackAllocationError {
                requested: size,
                active: self.active_bytes,
                cached: self.live_bytes,
            })
        })?;
        Ok(body)
    }

    fn base(&self, position: usize) -> Option<Arc<EncodedObject>> {
        let entry = &self.plan.scan.entries[position];
        match entry.base_position {
            Some(base) => self.states[base].object.clone(),
            None => match entry.public.base {
                Some(PackScanBase::External(oid)) => self
                    .external
                    .get(&oid)
                    .map(|(object, _)| Arc::clone(object)),
                _ => None,
            },
        }
    }

    fn consume_base(&mut self, position: usize) {
        let entry = &self.plan.scan.entries[position];
        if let Some(base) = entry.base_position {
            self.remaining[base] -= 1;
            if self.remaining[base] == 0 {
                if let Some(object) = &self.states[base].object {
                    self.live_bytes -= object.body.len();
                }
                if self.states[base].yielded {
                    if let Some(object) = self.states[base].object.take() {
                        self.active_bytes -= object.body.len();
                    }
                }
            }
        } else if let Some(PackScanBase::External(oid)) = entry.public.base {
            if let Some((object, remaining)) = self.external.get_mut(&oid) {
                *remaining -= 1;
                if *remaining == 0 {
                    let size = object.body.len();
                    self.external.remove(&oid);
                    self.live_bytes -= size;
                    self.active_bytes -= size;
                }
            }
        }
    }

    fn inflate_next(&mut self) -> ScanResult<()> {
        let position =
            *self.plan.planned.get(self.input).ok_or_else(|| {
                GitError::InvalidObject("planned entry has no resolved base".into())
            })?;
        let entry = &self.plan.scan.entries[position];
        let mut body = self.allocate(entry.public.declared_size)?;
        let (consumed, _) = inflate_source_exact_into(
            &self.plan.scan.source,
            entry.data_offset,
            entry.end,
            &mut body,
            entry.public.declared_size as usize,
            CancelFlag::never(),
        )?;
        if consumed != entry.end - entry.data_offset {
            return Err(GitError::InvalidObject(
                "compressed entry length differs from index span".into(),
            )
            .into());
        }
        self.input += 1;
        self.stats.entries_inflated += 1;
        self.stats.bytes_inflated = self.stats.bytes_inflated.saturating_add(body.len() as u64);
        self.active_bytes += body.len();
        self.states[position].delta = Some(body);
        let mut ready = VecDeque::from([position]);
        while let Some(position) = ready.pop_front() {
            if self.states[position].delta.is_none() {
                continue;
            }
            let kind = self.plan.scan.entries[position].public.kind;
            let oid = self.plan.scan.entries[position].public.oid;
            let object = match kind {
                PackScanKind::Object(kind) => EncodedObject::new(
                    kind,
                    self.states[position]
                        .delta
                        .take()
                        .ok_or_else(|| GitError::InvalidObject("missing inflated body".into()))?,
                ),
                _ => {
                    let Some(base) = self.base(position) else {
                        continue;
                    };
                    let delta = self.states[position].delta.as_ref().ok_or_else(|| {
                        GitError::InvalidObject("missing delta instructions".into())
                    })?;
                    let plan = plan_pack_delta(&base.body, delta)?;
                    let mut result = self.allocate(plan.result_size)?;
                    apply_pack_delta_exact(
                        &base.body,
                        delta,
                        plan,
                        &mut result,
                        CancelFlag::never(),
                    )?;
                    let delta_size = delta.len();
                    self.states[position].delta = None;
                    self.active_bytes = self.active_bytes - delta_size + result.len();
                    let kind = base.object_type;
                    drop(base);
                    self.consume_base(position);
                    EncodedObject::new(kind, result)
                }
            };
            if object.object_id(self.plan.scan.format)? != oid {
                return Err(GitError::InvalidObject(
                    "scanned object identity differs from index".into(),
                )
                .into());
            }
            if self.remaining[position] > 0 {
                self.live_bytes = self.live_bytes.saturating_add(object.body.len());
                check_limit(
                    PackLimitKind::LiveBaseBytes,
                    self.plan.scan.limits.max_cached_bytes,
                    self.live_bytes,
                )?;
                self.stats.peak_live_base_bytes =
                    self.stats.peak_live_base_bytes.max(self.live_bytes);
            }
            self.states[position].object = Some(Arc::new(object));
            ready.extend(self.plan.dependents[position].iter().copied());
        }
        Ok(())
    }
}

impl<S: PackReadSource> Iterator for PackScanCursor<'_, S> {
    type Item = ScanResult<PackScanObject>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let &position = self.plan.planned.get(self.output)?;
        while self.states[position].object.is_none() {
            if let Err(error) = self.inflate_next() {
                self.failed = true;
                self.states.clear();
                self.external.clear();
                return Some(Err(error));
            }
        }
        let state = &mut self.states[position];
        let object = if true {
            let object = state.object.take()?;
            self.active_bytes -= object.body.len();
            object
        } else {
            Arc::clone(state.object.as_ref()?)
        };
        state.yielded = true;
        self.output += 1;
        let entry = &self.plan.scan.entries[position].public;
        Some(Ok(PackScanObject {
            oid: entry.oid,
            offset: entry.offset,
            requested: self.plan.requested[position],
            object,
        }))
    }
}

impl<S: PackReadSource> std::iter::FusedIterator for PackScanCursor<'_, S> {}

fn check_limit(kind: PackLimitKind, limit: usize, attempted: usize) -> ScanResult<()> {
    if attempted > limit {
        Err(PackReadError::Limit(PackLimitError {
            kind,
            limit,
            attempted,
        }))
    } else {
        Ok(())
    }
}

fn read_exact(
    source: &impl PackReadSource,
    mut offset: u64,
    mut bytes: &mut [u8],
) -> ScanResult<()> {
    while !bytes.is_empty() {
        let read = match source.read_at(offset, bytes) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 || read > bytes.len() {
            return Err(GitError::InvalidFormat(
                "truncated pack or invalid source read length".into(),
            )
            .into());
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| GitError::InvalidFormat("pack source offset overflow".into()))?;
        bytes = &mut bytes[read..];
    }
    Ok(())
}
