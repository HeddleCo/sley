//! Delta indexing, planning, and git pack delta encoding.
//!
//! Split out of `lib.rs` in the W21 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;

/// Size, in bytes, of the fixed blocks used to index a base object for delta
/// compression. Matches git's `diff-delta.c` block size.
pub(crate) const DELTA_BLOCK_SIZE: usize = 16;

/// Distance between indexed base anchors. Delta generation still scans target
/// objects byte-by-byte once there is evidence of shared content; anchoring the
/// base at block boundaries keeps the index compact and avoids per-object
/// hash-table allocation storms on unrelated blobs.
pub(crate) const DELTA_INDEX_STRIDE: usize = DELTA_BLOCK_SIZE;

/// Number of hash buckets used by [`DeltaIndex`]. Bucketing avoids sorting each
/// base object's anchors while keeping exact-hash candidate scans short.
pub(crate) const DELTA_BUCKET_BITS: usize = 12;
pub(crate) const DELTA_BUCKET_COUNT: usize = 1 << DELTA_BUCKET_BITS;
pub(crate) const DELTA_BUCKET_MASK: usize = DELTA_BUCKET_COUNT - 1;

/// An index over a base object's content used to generate deltas against it.
///
/// The index hashes block-sized anchors of the base, groups them into fixed
/// buckets, and verifies exact byte matches before copying. This avoids both
/// per-bucket allocation storms and the per-object sort needed by a single
/// sorted vector.
pub(crate) struct DeltaIndex<'a> {
    base: &'a [u8],
    blocks: Vec<DeltaBlock>,
    buckets: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeltaBlock {
    hash: u32,
    offset: usize,
}

impl<'a> DeltaIndex<'a> {
    pub(crate) fn new(base: &'a [u8]) -> Self {
        let mut buckets = vec![0usize; DELTA_BUCKET_COUNT + 1];
        let mut anchors = Vec::with_capacity(delta_anchor_count(base.len()));
        for_each_delta_anchor(base.len(), |offset| {
            let hash = block_hash(&base[offset..offset + DELTA_BLOCK_SIZE]);
            buckets[delta_bucket(hash) + 1] += 1;
            anchors.push(DeltaBlock { hash, offset });
        });
        for idx in 1..buckets.len() {
            buckets[idx] += buckets[idx - 1];
        }

        let mut next_offsets = buckets[..DELTA_BUCKET_COUNT].to_vec();
        let mut blocks = vec![DeltaBlock { hash: 0, offset: 0 }; anchors.len()];
        for anchor in anchors {
            let bucket = delta_bucket(anchor.hash);
            let next = &mut next_offsets[bucket];
            blocks[*next] = anchor;
            *next += 1;
        }

        Self {
            base,
            blocks,
            buckets,
        }
    }

    pub(crate) fn candidate_blocks(&self, hash: u32) -> impl Iterator<Item = &DeltaBlock> {
        let bucket = delta_bucket(hash);
        let start = self.buckets[bucket];
        let end = self.buckets[bucket + 1];
        self.blocks[start..end]
            .iter()
            .filter(move |block| block.hash == hash)
    }

    pub(crate) fn has_hash(&self, hash: u32) -> bool {
        self.candidate_blocks(hash).next().is_some()
    }

    pub(crate) fn has_shared_anchor(&self, target: &[u8]) -> bool {
        if target.len() < DELTA_BLOCK_SIZE || self.blocks.is_empty() {
            return false;
        }
        let last = target.len() - DELTA_BLOCK_SIZE;
        for offset in (0..=last).step_by(DELTA_INDEX_STRIDE) {
            let hash = block_hash(&target[offset..offset + DELTA_BLOCK_SIZE]);
            if self.has_hash(hash) {
                return true;
            }
        }
        if !last.is_multiple_of(DELTA_INDEX_STRIDE) {
            let hash = block_hash(&target[last..last + DELTA_BLOCK_SIZE]);
            if self.has_hash(hash) {
                return true;
            }
        }
        false
    }

    /// Generate a delta that reconstructs `target` from this index's base.
    pub(crate) fn delta(&self, target: &[u8]) -> Option<Vec<u8>> {
        if !self.has_shared_anchor(target) {
            return None;
        }
        let base = self.base;
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base.len() as u64);
        write_delta_varint(&mut delta, target.len() as u64);

        let mut pending_insert_start = 0usize;
        let mut pos = 0usize;
        while pos < target.len() {
            let mut best_len = 0usize;
            let mut best_offset = 0usize;
            if pos + DELTA_BLOCK_SIZE <= target.len() {
                let hash = block_hash(&target[pos..pos + DELTA_BLOCK_SIZE]);
                for candidate in self.candidate_blocks(hash).take(DELTA_MAX_CHAIN) {
                    // Confirm the block actually matches (hash collisions are
                    // possible) before measuring how far it extends.
                    let candidate = candidate.offset;
                    let max_len = (base.len() - candidate).min(target.len() - pos);
                    let mut len = 0usize;
                    while len < max_len && base[candidate + len] == target[pos + len] {
                        len += 1;
                    }
                    if len > best_len {
                        best_len = len;
                        best_offset = candidate;
                    }
                }
            }

            if best_len >= DELTA_BLOCK_SIZE {
                if pending_insert_start < pos {
                    write_delta_insert(&mut delta, &target[pending_insert_start..pos]);
                }
                write_delta_copy(&mut delta, best_offset as u64, best_len as u64);
                pos += best_len;
                pending_insert_start = pos;
            } else {
                pos += 1;
            }
        }
        if pending_insert_start < target.len() {
            write_delta_insert(&mut delta, &target[pending_insert_start..]);
        }
        Some(delta)
    }
}

pub(crate) fn for_each_delta_anchor(mut len: usize, mut visit: impl FnMut(usize)) {
    if len < DELTA_BLOCK_SIZE {
        return;
    }
    len -= DELTA_BLOCK_SIZE;
    for offset in (0..=len).step_by(DELTA_INDEX_STRIDE) {
        visit(offset);
    }
    if !len.is_multiple_of(DELTA_INDEX_STRIDE) {
        visit(len);
    }
}

pub(crate) fn delta_anchor_count(len: usize) -> usize {
    if len < DELTA_BLOCK_SIZE {
        return 0;
    }
    let last = len - DELTA_BLOCK_SIZE;
    (last / DELTA_INDEX_STRIDE) + 1 + usize::from(!last.is_multiple_of(DELTA_INDEX_STRIDE))
}

pub(crate) fn delta_bucket(hash: u32) -> usize {
    (hash as usize) & DELTA_BUCKET_MASK
}

/// Maximum number of base offsets retained per block-hash bucket. Caps the work
/// done extending candidate matches for inputs with many repeated blocks.
pub(crate) const DELTA_MAX_CHAIN: usize = 64;

/// Hash a fixed-size block of base/target bytes into a bucket key.
///
/// A simple multiplicative (FNV-style) hash is sufficient here: matches are
/// always verified byte-for-byte before use, so collisions only cost a little
/// extra comparison work and never affect correctness.
pub(crate) fn block_hash(block: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &byte in block {
        hash = hash.wrapping_mul(0x0100_0193) ^ u32::from(byte);
    }
    hash
}

/// The chosen storage form for a single object during pack generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedBase {
    /// Stored undeltified (a base for others, or no good delta was found).
    None,
    /// Delta against another object in this pack, identified by its original
    /// index. The pre-computed `delta` bytes reconstruct the object from that
    /// base's body.
    InPack { base_idx: usize, delta: Vec<u8> },
    /// Delta against an external (thin-pack) base, referenced by object id.
    External { base_oid: ObjectId, delta: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedEntry {
    pub(crate) base: PlannedBase,
}

#[derive(Debug, Clone)]
pub(crate) struct StreamingDeltaBase {
    pub(crate) oid: ObjectId,
    pub(crate) object: Arc<EncodedObject>,
    pub(crate) offset: u64,
    pub(crate) depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamingPlannedBase {
    None,
    Current {
        base_idx: usize,
        delta: Vec<u8>,
    },
    Previous {
        base_oid: ObjectId,
        base_offset: u64,
        delta: Vec<u8>,
    },
    External {
        base_oid: ObjectId,
        delta: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamingPlannedEntry {
    pub(crate) base: StreamingPlannedBase,
    pub(crate) depth: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamingCandidateBase {
    Previous {
        oid: ObjectId,
        offset: u64,
        depth: usize,
    },
    Current {
        idx: usize,
        depth: usize,
    },
}

pub(crate) struct StreamingDeltaWindowEntry<'a> {
    base: StreamingCandidateBase,
    object_type: ObjectType,
    index: DeltaIndex<'a>,
}
/// Maximum number of external thin-pack bases compared against any single
/// object. Bounds the work of the thin path when a large base set is supplied.
pub(crate) const DELTA_MAX_EXTERNAL_BASES: usize = 64;

pub(crate) struct DeltaWindowEntry<'a> {
    idx: usize,
    index: DeltaIndex<'a>,
}

/// Rank object types for delta grouping. Objects of the same type are far more
/// likely to delta well, so the sort groups by this rank first.
pub(crate) fn delta_type_rank(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Commit => 0,
        ObjectType::Tree => 1,
        ObjectType::Blob => 2,
        ObjectType::Tag => 3,
    }
}

pub(crate) fn plan_streaming_window_deltas(
    objects: &[Arc<EncodedObject>],
    object_ids: &[ObjectId],
    base_horizon: &VecDeque<StreamingDeltaBase>,
    options: &PackWriteOptions,
) -> (Vec<StreamingPlannedEntry>, Vec<usize>) {
    let count = objects.len();
    let mut plan: Vec<StreamingPlannedEntry> = (0..count)
        .map(|_| StreamingPlannedEntry {
            base: StreamingPlannedBase::None,
            depth: 0,
        })
        .collect();

    let mut order: Vec<usize> = (0..count).collect();
    if options.reorder && options.depth > 0 {
        order.sort_by(|&left, &right| {
            delta_type_rank(objects[left].object_type)
                .cmp(&delta_type_rank(objects[right].object_type))
                .then_with(|| objects[right].body.len().cmp(&objects[left].body.len()))
                .then_with(|| {
                    object_ids[left]
                        .as_bytes()
                        .cmp(object_ids[right].as_bytes())
                })
        });
    }

    if options.depth == 0 || options.window == 0 {
        return (plan, order);
    }

    let mut external_indexes: Vec<(ObjectId, ObjectType, DeltaIndex<'_>)> =
        Vec::with_capacity(options.thin_bases.len());
    let mut external_bases = options.thin_bases.iter().collect::<Vec<_>>();
    external_bases
        .sort_by(|(left_oid, _), (right_oid, _)| left_oid.as_bytes().cmp(right_oid.as_bytes()));
    for (oid, object) in external_bases {
        external_indexes.push((*oid, object.object_type, DeltaIndex::new(&object.body)));
    }

    let mut window: VecDeque<StreamingDeltaWindowEntry<'_>> =
        VecDeque::with_capacity(options.window.min(base_horizon.len() + count));
    for base in base_horizon {
        window.push_back(StreamingDeltaWindowEntry {
            base: StreamingCandidateBase::Previous {
                oid: base.oid,
                offset: base.offset,
                depth: base.depth,
            },
            object_type: base.object.object_type,
            index: DeltaIndex::new(&base.object.body),
        });
    }
    while window.len() > options.window {
        window.pop_front();
    }

    for &idx in &order {
        let target = &objects[idx].body;
        let target_type = objects[idx].object_type;

        let mut best_delta: Option<Vec<u8>> = None;
        let mut best_base = StreamingPlannedBase::None;
        let mut best_base_depth = 0usize;

        for base_entry in window.iter().rev() {
            if base_entry.object_type != target_type {
                continue;
            }
            let base_depth = match &base_entry.base {
                StreamingCandidateBase::Previous { depth, .. }
                | StreamingCandidateBase::Current { depth, .. } => *depth,
            };
            if base_depth + 1 > options.depth {
                continue;
            }
            let Some(delta) = base_entry.index.delta(target) else {
                continue;
            };
            if !delta_is_acceptable(&delta, target.len()) {
                continue;
            }
            if best_delta
                .as_ref()
                .is_none_or(|current| delta.len() < current.len())
            {
                best_delta = Some(delta);
                best_base_depth = base_depth;
                best_base = match &base_entry.base {
                    StreamingCandidateBase::Previous { oid, offset, .. } => {
                        StreamingPlannedBase::Previous {
                            base_oid: *oid,
                            base_offset: *offset,
                            delta: Vec::new(),
                        }
                    }
                    StreamingCandidateBase::Current { idx: base_idx, .. } => {
                        StreamingPlannedBase::Current {
                            base_idx: *base_idx,
                            delta: Vec::new(),
                        }
                    }
                };
            }
        }

        for (base_oid, base_type, base_index) in
            external_indexes.iter().take(DELTA_MAX_EXTERNAL_BASES)
        {
            if *base_type != target_type {
                continue;
            }
            let Some(delta) = base_index.delta(target) else {
                continue;
            };
            if !delta_is_acceptable(&delta, target.len()) {
                continue;
            }
            if best_delta
                .as_ref()
                .is_none_or(|current| delta.len() < current.len())
            {
                best_delta = Some(delta);
                best_base_depth = 0;
                best_base = StreamingPlannedBase::External {
                    base_oid: *base_oid,
                    delta: Vec::new(),
                };
            }
        }

        if let Some(delta) = best_delta {
            plan[idx].depth = best_base_depth + 1;
            plan[idx].base = match best_base {
                StreamingPlannedBase::Current { base_idx, .. } => {
                    StreamingPlannedBase::Current { base_idx, delta }
                }
                StreamingPlannedBase::Previous {
                    base_oid,
                    base_offset,
                    ..
                } => StreamingPlannedBase::Previous {
                    base_oid,
                    base_offset,
                    delta,
                },
                StreamingPlannedBase::External { base_oid, .. } => {
                    StreamingPlannedBase::External { base_oid, delta }
                }
                StreamingPlannedBase::None => StreamingPlannedBase::None,
            };
        }

        window.push_back(StreamingDeltaWindowEntry {
            base: StreamingCandidateBase::Current {
                idx,
                depth: plan[idx].depth,
            },
            object_type: objects[idx].object_type,
            index: DeltaIndex::new(&objects[idx].body),
        });
        while window.len() > options.window {
            window.pop_front();
        }
    }

    (plan, order)
}

/// Decide how each object is stored (undeltified or deltified) and the order in
/// which objects are emitted into the pack.
///
/// # Ordering
///
/// Candidates are sorted by `(type, size descending, object id)`:
/// * **type** — only same-type objects are deltified against one another, so
///   grouping by type keeps the sliding window full of viable bases. Type rank
///   follows [`delta_type_rank`] (commit, tree, blob, tag).
/// * **size descending** — larger objects come first so smaller, later objects
///   delta against larger bases (git's heuristic). Raw [`EncodedObject`]s carry
///   no path/name, so the usual path-hash key is unavailable; size is the next
///   best locality signal.
/// * **object id** — a deterministic tiebreaker for reproducible packs.
///
/// # Selection
///
/// Each object is compared against the previous up to `window` same-type
/// candidates (and, for thin packs, up to [`DELTA_MAX_EXTERNAL_BASES`] external
/// bases of the same type). The smallest delta whose encoded length is strictly
/// less than the object's own body is kept; otherwise the object is stored
/// undeltified. Delta chain depth is bounded by `options.depth` (a base may
/// only be used if doing so keeps the resulting chain within the bound); a depth
/// of `0` disables deltification entirely.
///
/// Returns the per-object plan (indexed by original object index) together with
/// the emit order. Every in-pack delta references a candidate that is earlier in
/// the emit order, so emitting in that order writes each base before any object
/// that depends on it.
pub(crate) fn plan_pack_deltas(
    objects: &[&EncodedObject],
    object_ids: &[ObjectId],
    options: &PackWriteOptions,
) -> Result<(Vec<PlannedEntry>, Vec<usize>)> {
    let count = objects.len();
    let mut plan: Vec<PlannedEntry> = (0..count)
        .map(|_| PlannedEntry {
            base: PlannedBase::None,
        })
        .collect();

    // Processing order. Deltas only point backwards within this order, which is
    // therefore also a valid emit order. Reordering by type/size improves delta
    // locality but is skipped when disabled or when deltification is off.
    let mut order: Vec<usize> = (0..count).collect();
    if options.reorder && options.depth > 0 {
        order.sort_by(|&left, &right| {
            delta_type_rank(objects[left].object_type)
                .cmp(&delta_type_rank(objects[right].object_type))
                .then_with(|| objects[right].body.len().cmp(&objects[left].body.len()))
                .then_with(|| {
                    object_ids[left]
                        .as_bytes()
                        .cmp(object_ids[right].as_bytes())
                })
        });
    }

    if options.depth == 0 {
        return Ok((plan, order));
    }

    // Pre-build delta indexes for external thin-pack bases, grouped by type so
    // an object only compares against compatible bases.
    let mut external_indexes: Vec<(ObjectId, ObjectType, DeltaIndex<'_>)> =
        Vec::with_capacity(options.thin_bases.len());
    for (oid, object) in &options.thin_bases {
        external_indexes.push((*oid, object.object_type, DeltaIndex::new(&object.body)));
    }

    // Chain depth ending at each object (0 = undeltified). Used to keep delta
    // chains within `options.depth`.
    let mut depth = vec![0usize; count];
    // Sliding window of recently processed original indices, most recent last.
    let mut window: std::collections::VecDeque<DeltaWindowEntry<'_>> =
        std::collections::VecDeque::new();

    for &idx in &order {
        let target = &objects[idx].body;
        let target_type = objects[idx].object_type;

        let mut best_delta: Option<Vec<u8>> = None;
        let mut best_base = PlannedBase::None;

        // Try in-pack candidates from the window (same type only).
        for base_entry in window.iter().rev() {
            let base_idx = base_entry.idx;
            if objects[base_idx].object_type != target_type {
                continue;
            }
            // Using this base would make the new chain depth + 1; skip if that
            // would exceed the configured maximum.
            if depth[base_idx] + 1 > options.depth {
                continue;
            }
            let Some(delta) = base_entry.index.delta(target) else {
                continue;
            };
            if !delta_is_acceptable(&delta, target.len()) {
                continue;
            }
            if best_delta
                .as_ref()
                .is_none_or(|current| delta.len() < current.len())
            {
                best_delta = Some(delta);
                best_base = PlannedBase::InPack {
                    base_idx,
                    delta: Vec::new(),
                };
            }
        }

        // Try external thin-pack bases (ref-delta; external base is depth 0, so
        // the resulting chain depth is 1, always within a non-zero bound).
        for (base_oid, base_type, base_index) in
            external_indexes.iter().take(DELTA_MAX_EXTERNAL_BASES)
        {
            if *base_type != target_type {
                continue;
            }
            let Some(delta) = base_index.delta(target) else {
                continue;
            };
            if !delta_is_acceptable(&delta, target.len()) {
                continue;
            }
            if best_delta
                .as_ref()
                .is_none_or(|current| delta.len() < current.len())
            {
                best_delta = Some(delta);
                best_base = PlannedBase::External {
                    base_oid: *base_oid,
                    delta: Vec::new(),
                };
            }
        }

        if let Some(delta) = best_delta {
            match best_base {
                PlannedBase::InPack { base_idx, .. } => {
                    depth[idx] = depth[base_idx] + 1;
                    plan[idx].base = PlannedBase::InPack { base_idx, delta };
                }
                PlannedBase::External { base_oid, .. } => {
                    depth[idx] = 1;
                    plan[idx].base = PlannedBase::External { base_oid, delta };
                }
                PlannedBase::None => {}
            }
        }

        // Add this object to the window for subsequent candidates.
        window.push_back(DeltaWindowEntry {
            idx,
            index: DeltaIndex::new(&objects[idx].body),
        });
        while window.len() > options.window {
            window.pop_front();
        }
    }

    Ok((plan, order))
}

/// Whether a generated delta is worth using instead of storing the object
/// undeltified. The encoded delta must be strictly smaller than the object's own
/// body; otherwise the undeltified form is the same size or smaller and is
/// always self-contained.
pub(crate) fn delta_is_acceptable(delta: &[u8], target_len: usize) -> bool {
    !delta.is_empty() && delta.len() < target_len
}

pub(crate) fn write_delta_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value as u8) & 0x7f;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

pub(crate) fn write_delta_copy(out: &mut Vec<u8>, mut offset: u64, mut size: u64) {
    while size != 0 {
        let chunk = size.min(0x10000);
        let encoded_size = if chunk == 0x10000 { 0 } else { chunk };
        let mut command = 0x80u8;
        let mut payload = [0u8; 7];
        let mut payload_len = 0usize;
        for idx in 0..4 {
            let byte = ((offset >> (idx * 8)) & 0xff) as u8;
            if byte != 0 {
                command |= 1 << idx;
                payload[payload_len] = byte;
                payload_len += 1;
            }
        }
        for idx in 0..3 {
            let byte = ((encoded_size >> (idx * 8)) & 0xff) as u8;
            if byte != 0 {
                command |= 0x10 << idx;
                payload[payload_len] = byte;
                payload_len += 1;
            }
        }
        out.push(command);
        out.extend_from_slice(&payload[..payload_len]);
        offset += chunk;
        size -= chunk;
    }
}

pub(crate) fn write_delta_insert(out: &mut Vec<u8>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let chunk_len = bytes.len().min(0x7f);
        out.push(chunk_len as u8);
        out.extend_from_slice(&bytes[..chunk_len]);
        bytes = &bytes[chunk_len..];
    }
}
