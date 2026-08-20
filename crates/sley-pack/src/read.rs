//! Pack parsing and random-access object/header reads (including delta resolution).
//!
//! Split out of `lib.rs` in the W21 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;

impl PackFile {
    pub fn parse_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse_sha1_with_limits(bytes, PackReadLimits::default())
    }

    pub fn parse_sha1_with_limits(bytes: &[u8], limits: PackReadLimits) -> Result<Self> {
        Self::parse_with_limits(bytes, ObjectFormat::Sha1, limits)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_with_limits(bytes, format, PackReadLimits::default())
    }

    /// Parse and resolve a complete pack with explicit read limits.
    pub fn parse_with_limits(
        bytes: &[u8],
        format: ObjectFormat,
        limits: PackReadLimits,
    ) -> Result<Self> {
        Self::parse_with_base_and_limits(bytes, format, |_| Ok(None), limits)
    }

    pub fn parse_bundle(bundle: &Bundle) -> Result<Self> {
        Self::parse_bundle_with_limits(bundle, PackReadLimits::default())
    }

    pub fn parse_bundle_with_limits(bundle: &Bundle, limits: PackReadLimits) -> Result<Self> {
        Self::parse_with_limits(&bundle.pack, bundle.format, limits)
    }

    pub fn index_pack(bytes: &[u8], format: ObjectFormat) -> Result<PackWrite> {
        Self::index_pack_with_limits(bytes, format, PackReadLimits::default())
    }

    pub fn index_pack_with_limits(
        bytes: &[u8],
        format: ObjectFormat,
        limits: PackReadLimits,
    ) -> Result<PackWrite> {
        let PackIndexBuild {
            index,
            pack_checksum,
            entries,
        } = PackIndex::write_v2_for_pack_with_limits(bytes, format, limits)?;
        Ok(PackWrite {
            pack: bytes.to_vec(),
            index,
            checksum: pack_checksum,
            entries,
            delta_count: 0,
        })
    }

    pub fn parse_thin<F>(bytes: &[u8], format: ObjectFormat, external_base: F) -> Result<Self>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        Self::parse_thin_with_limits(bytes, format, external_base, PackReadLimits::default())
    }

    pub fn parse_thin_with_limits<F>(
        bytes: &[u8],
        format: ObjectFormat,
        external_base: F,
        limits: PackReadLimits,
    ) -> Result<Self>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        Self::parse_with_base_and_limits(bytes, format, external_base, limits)
    }

    pub(crate) fn parse_with_base_and_limits<F>(
        bytes: &[u8],
        format: ObjectFormat,
        mut external_base: F,
        limits: PackReadLimits,
    ) -> Result<Self>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        let trailer_len = format.raw_len();
        if bytes.len() < 12 + trailer_len {
            return Err(GitError::InvalidFormat("pack file too short".into()));
        }
        let trailer_offset = bytes.len() - trailer_len;
        let entry_region = pack_entry_region(bytes, trailer_offset)?;
        let checksum = sley_core::digest_bytes(format, entry_region)?;
        let expected = ObjectId::from_raw(format, &bytes[trailer_offset..])?;
        if checksum != expected {
            return Err(GitError::InvalidFormat(format!(
                "pack checksum mismatch: expected {expected}, got {checksum}"
            )));
        }

        if &bytes[..4] != b"PACK" {
            return Err(GitError::InvalidFormat("missing PACK signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 2 && version != 3 {
            return Err(GitError::Unsupported(format!("pack version {version}")));
        }
        // sley#4: the declared count is attacker-controlled; validate it against
        // the bytes that actually remain before reserving anything for it.
        let count = checked_pack_object_count(
            u32_be(&bytes[8..12]),
            (trailer_offset.saturating_sub(12)) as u64,
        )?;
        let mut offset = 12usize;
        let mut entries = Vec::with_capacity(pack_entry_prealloc(count));
        for _ in 0..count {
            let entry_offset = offset;
            let header = parse_entry_header(entry_region, &mut offset)?;
            let base = match header.kind {
                PackObjectKind::OfsDelta => Some(DeltaBase::Offset(parse_ofs_delta_base_offset(
                    entry_region,
                    &mut offset,
                    entry_offset as u64,
                )?)),
                PackObjectKind::RefDelta => {
                    let hash_len = format.raw_len();
                    if offset + hash_len > trailer_offset {
                        return Err(GitError::InvalidFormat(
                            "truncated ref-delta base object id".into(),
                        ));
                    }
                    let oid = ObjectId::from_raw(format, &entry_region[offset..offset + hash_len])?;
                    offset += hash_len;
                    Some(DeltaBase::Ref(oid))
                }
                _ => None,
            };
            let mut body = Vec::new();
            let consumed = inflate_into(
                &entry_region[offset..],
                &mut body,
                header.size.min(usize::MAX as u64) as usize,
            )?;
            if body.len() as u64 != header.size {
                return Err(GitError::InvalidObject(format!(
                    "pack object declared {} bytes, decoded {}",
                    header.size,
                    body.len()
                )));
            }
            if consumed == 0 {
                return Err(GitError::InvalidFormat(
                    "empty compressed pack entry".into(),
                ));
            }
            offset = offset
                .checked_add(consumed)
                .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
            if offset > trailer_offset {
                return Err(GitError::InvalidFormat(
                    "pack entry extends past checksum".into(),
                ));
            }
            if let Some(base) = base {
                entries.push(ParsedPackEntry::Delta {
                    base,
                    compressed_size: consumed as u64,
                    delta_size: header.size,
                    offset: entry_offset as u64,
                    delta: body,
                });
            } else {
                let object_type = match header.kind {
                    PackObjectKind::Commit => ObjectType::Commit,
                    PackObjectKind::Tree => ObjectType::Tree,
                    PackObjectKind::Blob => ObjectType::Blob,
                    PackObjectKind::Tag => ObjectType::Tag,
                    PackObjectKind::OfsDelta | PackObjectKind::RefDelta => unreachable!(),
                };
                let object = EncodedObject::new(object_type, body);
                let oid = object.object_id(format)?;
                entries.push(ParsedPackEntry::Resolved(PackObject {
                    entry: PackEntry {
                        oid,
                        compressed_size: consumed as u64,
                        uncompressed_size: header.size,
                        offset: entry_offset as u64,
                    },
                    object,
                }));
            }
        }
        if offset != trailer_offset {
            return Err(GitError::InvalidFormat(format!(
                "pack has {} trailing bytes before checksum",
                trailer_offset - offset
            )));
        }
        Ok(Self {
            version,
            entries: resolve_pack_entries(entries, format, &mut external_base, limits)?,
            checksum,
        })
    }

    /// Walk the pack and produce per-object statistics matching the output of
    /// `git verify-pack -v` / `git index-pack --verify-stat`.
    ///
    /// Objects are returned in pack offset order (the order `git verify-pack -v`
    /// prints them). Each entry carries the *resolved* object id, type and size,
    /// the in-pack byte span (`size_in_pack` = the offset delta to the next
    /// object, or to the trailing checksum for the last object), the in-pack
    /// offset, the delta chain depth (`0` for undeltified objects), and — for
    /// deltas — the object id of the *immediate* base (which may itself be a
    /// delta). This mirrors `builtin/index-pack.c`'s `show_pack_info`.
    pub fn verify_pack_stats(bytes: &[u8], format: ObjectFormat) -> Result<PackVerifyStats> {
        Self::verify_pack_stats_with_limits(bytes, format, PackReadLimits::default())
    }

    pub fn verify_pack_stats_with_limits(
        bytes: &[u8],
        format: ObjectFormat,
        limits: PackReadLimits,
    ) -> Result<PackVerifyStats> {
        // Resolve the whole pack first: this validates the trailing checksum,
        // every object's inflate, and yields the resolved oid/type/size keyed by
        // offset. `verify-pack` is exactly this validation plus the stat report.
        let pack = Self::parse_with_limits(bytes, format, limits)?;

        // Independently walk the on-disk entries to recover each object's stored
        // kind and (for deltas) its base reference — information `PackFile`
        // discards once deltas are resolved.
        let trailer_len = format.raw_len();
        let trailer_offset = bytes.len() - trailer_len;
        let entry_region = pack_entry_region(bytes, trailer_offset)?;
        let count = checked_pack_object_count(
            u32_be(&bytes[8..12]),
            (trailer_offset.saturating_sub(12)) as u64,
        )?;
        let mut offset = 12usize;
        // Per entry in read (offset) order: (offset, base, on-disk stream size).
        // The stream size is what git prints in the size column: it is the
        // resolved object size for an undeltified entry, but the *delta
        // instruction stream* length for a delta entry (builtin/index-pack.c sets
        // `obj->size` from the entry header, before any delta is applied).
        let mut on_disk: Vec<OnDiskEntry> = Vec::with_capacity(pack_entry_prealloc(count));
        for _ in 0..count {
            let entry_offset = offset as u64;
            let header = parse_entry_header(entry_region, &mut offset)?;
            let stream_size = header.size;
            let base = match header.kind {
                PackObjectKind::OfsDelta => Some(DeltaBase::Offset(parse_ofs_delta_base_offset(
                    entry_region,
                    &mut offset,
                    entry_offset,
                )?)),
                PackObjectKind::RefDelta => {
                    let hash_len = format.raw_len();
                    if offset + hash_len > trailer_offset {
                        return Err(GitError::InvalidFormat(
                            "truncated ref-delta base object id".into(),
                        ));
                    }
                    let oid = ObjectId::from_raw(format, &entry_region[offset..offset + hash_len])?;
                    offset += hash_len;
                    Some(DeltaBase::Ref(oid))
                }
                _ => None,
            };
            // Skip the compressed body to reach the next entry header.
            let mut body = Vec::new();
            let consumed = inflate_into(
                &entry_region[offset..],
                &mut body,
                header.size.min(usize::MAX as u64) as usize,
            )?;
            offset = offset
                .checked_add(consumed)
                .ok_or_else(|| GitError::InvalidFormat("pack offset overflow".into()))?;
            on_disk.push(OnDiskEntry {
                offset: entry_offset,
                base,
                stream_size,
            });
        }

        // Map offset -> resolved object so the on-disk walk can join in oid/type.
        let mut resolved_by_offset: HashMap<u64, &PackObject> =
            HashMap::with_capacity(pack.entries.len());
        for object in &pack.entries {
            resolved_by_offset.insert(object.entry.offset, object);
        }
        // Map offset -> resolved oid, for ofs-delta base lookups.
        let mut oid_by_offset: HashMap<u64, ObjectId> = HashMap::with_capacity(on_disk.len());
        for entry in &on_disk {
            if let Some(object) = resolved_by_offset.get(&entry.offset) {
                oid_by_offset.insert(entry.offset, object.entry.oid);
            }
        }
        // Map base offset -> index in `on_disk`, for delta-depth propagation.
        let mut index_by_offset: HashMap<u64, usize> = HashMap::with_capacity(on_disk.len());
        for (idx, entry) in on_disk.iter().enumerate() {
            index_by_offset.insert(entry.offset, idx);
        }

        // Sorted offsets give the size-in-pack span (next offset - this offset),
        // with the trailing checksum offset as the final sentinel.
        let mut sorted_offsets: Vec<u64> = on_disk.iter().map(|entry| entry.offset).collect();
        sorted_offsets.sort_unstable();
        let mut next_offset: HashMap<u64, u64> = HashMap::with_capacity(sorted_offsets.len());
        for window in sorted_offsets.windows(2) {
            next_offset.insert(window[0], window[1]);
        }
        if let Some(last) = sorted_offsets.last() {
            next_offset.insert(*last, trailer_offset as u64);
        }

        // Compute delta depth by following base offsets. Depth of a non-delta is
        // 0; a delta's depth is its base's depth + 1. `index_by_offset` lets an
        // ofs-delta find its base's index; a ref-delta resolves its base oid to
        // an in-pack offset when present (thin-pack external bases are not stored
        // in this pack, but verify-pack only ever runs on self-contained packs).
        let mut depth = vec![None; on_disk.len()];
        fn resolve_depth(
            idx: usize,
            on_disk: &[OnDiskEntry],
            index_by_offset: &HashMap<u64, usize>,
            offset_of_oid: &HashMap<ObjectId, u64>,
            depth: &mut [Option<u32>],
        ) -> u32 {
            if let Some(d) = depth[idx] {
                return d;
            }
            let computed = match &on_disk[idx].base {
                None => 0,
                Some(base) => {
                    let base_idx = match base {
                        DeltaBase::Offset(off) => index_by_offset.get(off).copied(),
                        DeltaBase::Ref(oid) => offset_of_oid
                            .get(oid)
                            .and_then(|off| index_by_offset.get(off).copied()),
                    };
                    match base_idx {
                        Some(bi) => {
                            resolve_depth(bi, on_disk, index_by_offset, offset_of_oid, depth) + 1
                        }
                        // Base not in this pack (thin pack); treat as depth 1.
                        None => 1,
                    }
                }
            };
            depth[idx] = Some(computed);
            computed
        }
        let mut offset_of_oid: HashMap<ObjectId, u64> = HashMap::with_capacity(oid_by_offset.len());
        for (off, oid) in &oid_by_offset {
            offset_of_oid.insert(*oid, *off);
        }
        for idx in 0..on_disk.len() {
            resolve_depth(idx, &on_disk, &index_by_offset, &offset_of_oid, &mut depth);
        }

        let mut stats = Vec::with_capacity(on_disk.len());
        for (idx, entry) in on_disk.iter().enumerate() {
            let off = entry.offset;
            let object = resolved_by_offset.get(&off).ok_or_else(|| {
                GitError::InvalidFormat("pack offset missing from resolved set".into())
            })?;
            let size_in_pack = next_offset
                .get(&off)
                .copied()
                .unwrap_or(trailer_offset as u64)
                .saturating_sub(off);
            let base_oid = match &entry.base {
                None => None,
                Some(DeltaBase::Offset(base_off)) => oid_by_offset.get(base_off).copied(),
                Some(DeltaBase::Ref(oid)) => Some(*oid),
            };
            stats.push(PackVerifyStat {
                oid: object.entry.oid,
                object_type: object.object.object_type,
                // git prints the on-disk stream size: object body size for an
                // undeltified entry, delta-instruction stream size for a delta.
                size: entry.stream_size,
                size_in_pack,
                offset: off,
                delta_depth: depth[idx].unwrap_or(0),
                base_oid,
            });
        }
        // Emit in pack offset order, matching git's read order.
        stats.sort_by_key(|stat| stat.offset);

        Ok(PackVerifyStats {
            objects: stats,
            checksum: pack.checksum,
        })
    }
}

/// A cache of objects already decoded from one specific pack, keyed by the
/// in-pack byte offset at which each object's entry begins.
///
/// Delta resolution within a pack walks a chain of base objects by offset; the
/// same base is the parent of many deltas, so without a cache the entire chain
/// is re-inflated and re-applied on every read. Implementors let
/// [`read_object_at_with_cache_arc`] reuse a warm base instead.
///
/// Correctness contract: a given `offset` within a given pack's bytes always
/// decodes to exactly one object, so caching by offset can never serve the wrong
/// object **provided the same cache is only ever used with one pack's bytes**.
/// Callers must therefore scope a cache to a single pack (e.g. key it by pack
/// path). The default [`read_object_at_arc`] uses a no-op cache and is unaffected.
pub trait PackDeltaCache {
    /// Return the decoded object whose entry begins at `offset`, if cached.
    fn get(&self, offset: u64) -> Option<Arc<EncodedObject>>;
    /// Record that the entry beginning at `offset` decodes to `object`.
    fn insert(&self, offset: u64, object: Arc<EncodedObject>);
}

/// A [`PackDeltaCache`] that stores nothing; used by [`read_object_at_arc`] to keep
/// the original, allocation-free behavior for callers that do not opt in.
pub(crate) struct NoopDeltaCache;

impl PackDeltaCache for NoopDeltaCache {
    fn get(&self, _offset: u64) -> Option<Arc<EncodedObject>> {
        None
    }
    fn insert(&self, _offset: u64, _object: Arc<EncodedObject>) {}
}

// Reused zlib inflate state. Resetting and reusing one `Decompress` avoids
// allocating a fresh (~10 KiB) `InflateState` for every object and delta decoded —
// an allocation that dominated bulk reads. Borrowed only for the duration of a
// single inflate; the recursive pack reader fully inflates each entry's data before
// recursing to its base, so the borrow never nests.
thread_local! {
    static INFLATE: RefCell<flate2::Decompress> = RefCell::new(flate2::Decompress::new(true));
}

/// The largest ratio by which a single DEFLATE/zlib member can expand its input.
/// The theoretical worst case for raw DEFLATE is ~1032:1 (a maximally efficient
/// run of back-references). We pre-reserve no more than this multiple of the
/// available compressed input, so an attacker who declares a huge `size_hint`
/// (e.g. `u64::MAX`) cannot make us reserve — and thus commit — gigabytes of
/// memory before the inflate has produced a single byte. The stream's *actual*
/// output is still verified against the declared size by the caller; this only
/// bounds the speculative allocation. git never pre-allocates an attacker's
/// declared size beyond a streaming buffer either (see index-pack.c's
/// `unpack_entry_data`).
///
/// Inflate the entire zlib stream at the front of `compressed`, appending the
/// decoded bytes to `out`, reusing the thread-local inflate state. `size_hint`
/// is the caller's expectation for the decompressed length, but it is treated as
/// untrusted: the up-front reservation is bounded by [`inflate::bounded_inflate_reserve`]
/// so a crafted hint can never drive an out-of-memory pre-allocation. Returns the
/// number of *compressed* bytes consumed (so callers stepping through a pack can
/// advance to the next entry). Byte-for-byte equivalent to
/// `ZlibDecoder::read_to_end` + `total_in`.
pub(crate) fn inflate_into(
    compressed: &[u8],
    out: &mut Vec<u8>,
    size_hint: usize,
) -> Result<usize> {
    INFLATE.with(|cell| {
        let mut decompress = cell.borrow_mut();
        decompress.reset(true);
        out.reserve(inflate::bounded_inflate_reserve(
            size_hint,
            compressed.len(),
        ));
        let mut input = compressed;
        let mut consumed_total = 0usize;
        loop {
            // Always leave output room so a zero-progress result means the input
            // (not the buffer) is exhausted.
            if out.len() == out.capacity() {
                out.reserve(out.len().max(64));
            }
            let before_in = decompress.total_in();
            let before_out = decompress.total_out();
            let status = decompress
                .decompress_vec(input, out, flate2::FlushDecompress::None)
                .map_err(|err| GitError::InvalidObject(format!("zlib inflate failed: {err}")))?;
            let consumed = (decompress.total_in() - before_in) as usize;
            let produced = decompress.total_out() - before_out;
            input = &input[consumed..];
            consumed_total += consumed;
            match status {
                flate2::Status::StreamEnd => return Ok(consumed_total),
                _ if consumed == 0 && produced == 0 => {
                    return Err(GitError::InvalidObject("truncated zlib stream".into()));
                }
                _ => {}
            }
        }
    })
}

/// Inflate at most `out.len()` bytes (or until the stream ends) from `compressed`
/// into `out`, reusing the thread-local state. Used to read a delta's leading
/// base-size / result-size varints without inflating the whole instruction stream
/// or allocating a heap prefix buffer (sley#26).
pub(crate) fn inflate_prefix(compressed: &[u8], out: &mut [u8]) -> Result<usize> {
    INFLATE.with(|cell| {
        let mut decompress = cell.borrow_mut();
        decompress.reset(true);
        let mut input = compressed;
        let mut written = 0usize;
        while written < out.len() {
            let before_in = decompress.total_in();
            let before_out = decompress.total_out();
            let status = decompress
                .decompress(input, &mut out[written..], flate2::FlushDecompress::None)
                .map_err(|err| GitError::InvalidObject(format!("zlib inflate failed: {err}")))?;
            let consumed = (decompress.total_in() - before_in) as usize;
            let produced = (decompress.total_out() - before_out) as usize;
            input = &input[consumed..];
            written = written.saturating_add(produced);
            if status == flate2::Status::StreamEnd || (consumed == 0 && produced == 0) {
                break;
            }
        }
        Ok(written)
    })
}
/// Decode the single object stored at byte `offset` within `pack_bytes`, reading
/// only that object and its delta-base chain instead of parsing the whole pack.
///
/// Ofs-delta bases are followed by offset (recursively, within this pack);
/// ref-delta bases are obtained from `resolve_ref_base`, which the caller backs
/// with the surrounding object store (so a base in another pack or loose still
/// resolves). The pack trailer checksum is the final `format.raw_len()` bytes.
pub fn read_object_at_arc<F>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    resolve_ref_base: F,
) -> Result<Arc<EncodedObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<Arc<EncodedObject>>>,
{
    read_object_at_with_cache_arc(
        pack_bytes,
        offset,
        format,
        resolve_ref_base,
        &NoopDeltaCache,
    )
}

/// Like [`read_object_at_arc`], but reuses already-decoded objects from `cache`
/// (keyed by in-pack offset) and records every object it decodes.
///
/// This turns repeated reads from the same pack — where many deltas share a base
/// chain — from re-inflating each chain per read into resolving each base once.
/// `cache` must be scoped to the pack `pack_bytes` belongs to (see
/// [`PackDeltaCache`]). The decoded object is returned behind an [`Arc`] so
/// callers can reuse cache handles without cloning full object bodies.
pub fn read_object_at_with_cache_arc<F, C>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    mut resolve_ref_base: F,
    cache: &C,
) -> Result<Arc<EncodedObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<Arc<EncodedObject>>>,
    C: PackDeltaCache + ?Sized,
{
    read_object_at_with_cache_and_ofs_base_arc(
        pack_bytes,
        offset,
        format,
        &mut resolve_ref_base,
        |_offset| Ok(None),
        cache,
    )
}

/// Like [`read_object_at_with_cache_arc`], but lets an object-database caller
/// recover an ofs-delta base from another storage copy when the in-pack base
/// offset cannot be decoded. Direct pack verification should keep using the
/// strict APIs; this hook mirrors normal object lookup, where a corrupt packed
/// copy does not hide a good loose or redundant packed copy.
pub fn read_object_at_with_cache_and_ofs_base_arc<F, G, C>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    mut resolve_ref_base: F,
    mut resolve_ofs_base: G,
    cache: &C,
) -> Result<Arc<EncodedObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<Arc<EncodedObject>>>,
    G: FnMut(u64) -> Result<Option<Arc<EncodedObject>>>,
    C: PackDeltaCache + ?Sized,
{
    read_object_at_inner(
        pack_bytes,
        offset,
        format,
        &mut resolve_ref_base,
        &mut resolve_ofs_base,
        cache,
    )
}

/// Like [`read_object_at_with_cache_and_ofs_base_arc`], without an offset-cache.
pub fn read_object_at_with_ofs_base_arc<F, G>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    resolve_ref_base: F,
    resolve_ofs_base: G,
) -> Result<Arc<EncodedObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<Arc<EncodedObject>>>,
    G: FnMut(u64) -> Result<Option<Arc<EncodedObject>>>,
{
    read_object_at_with_cache_and_ofs_base_arc(
        pack_bytes,
        offset,
        format,
        resolve_ref_base,
        resolve_ofs_base,
        &NoopDeltaCache,
    )
}

pub(crate) fn read_object_at_inner<F, G, C>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    resolve_ref_base: &mut F,
    resolve_ofs_base: &mut G,
    cache: &C,
) -> Result<Arc<EncodedObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<Arc<EncodedObject>>>,
    G: FnMut(u64) -> Result<Option<Arc<EncodedObject>>>,
    C: PackDeltaCache + ?Sized,
{
    // A warm cache entry for this exact offset is already the fully resolved
    // object, so the whole base chain below can be skipped.
    if let Some(object) = cache.get(offset) {
        return Ok(object);
    }
    let trailer_offset = pack_bytes
        .len()
        .checked_sub(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("pack smaller than its trailer".into()))?;
    let entry_region = pack_entry_region(pack_bytes, trailer_offset)?;
    let mut cursor = usize::try_from(offset)
        .ok()
        .filter(|&value| value < trailer_offset)
        .ok_or_else(|| GitError::InvalidFormat("pack object offset out of range".into()))?;
    let header = parse_entry_header(entry_region, &mut cursor)?;
    let base = match header.kind {
        PackObjectKind::OfsDelta => Some(DeltaBase::Offset(parse_ofs_delta_base_offset(
            entry_region,
            &mut cursor,
            offset,
        )?)),
        PackObjectKind::RefDelta => {
            let hash_len = format.raw_len();
            if cursor + hash_len > trailer_offset {
                return Err(GitError::InvalidFormat(
                    "truncated ref-delta base object id".into(),
                ));
            }
            let oid = ObjectId::from_raw(format, &entry_region[cursor..cursor + hash_len])?;
            cursor += hash_len;
            Some(DeltaBase::Ref(oid))
        }
        _ => None,
    };
    let mut body = Vec::new();
    inflate_into(
        &entry_region[cursor..],
        &mut body,
        header.size.min(usize::MAX as u64) as usize,
    )?;
    if body.len() as u64 != header.size {
        return Err(GitError::InvalidObject(format!(
            "pack object declared {} bytes, decoded {}",
            header.size,
            body.len()
        )));
    }
    let object = match base {
        None => {
            let object_type = match header.kind {
                PackObjectKind::Commit => ObjectType::Commit,
                PackObjectKind::Tree => ObjectType::Tree,
                PackObjectKind::Blob => ObjectType::Blob,
                PackObjectKind::Tag => ObjectType::Tag,
                PackObjectKind::OfsDelta | PackObjectKind::RefDelta => {
                    return Err(GitError::InvalidFormat(
                        "delta pack entry decoded without a base".into(),
                    ));
                }
            };
            Arc::new(EncodedObject::new(object_type, body))
        }
        Some(DeltaBase::Offset(base_offset)) => {
            let base = match read_object_at_inner(
                pack_bytes,
                base_offset,
                format,
                resolve_ref_base,
                resolve_ofs_base,
                cache,
            ) {
                Ok(base) => base,
                Err(pack_err) => match resolve_ofs_base(base_offset)? {
                    Some(base) => base,
                    None => return Err(pack_err),
                },
            };
            let resolved = apply_pack_delta(&base.body, &body)?;
            Arc::new(EncodedObject::new(base.object_type, resolved))
        }
        Some(DeltaBase::Ref(base_oid)) => {
            let base = resolve_ref_base(&base_oid)?
                .ok_or_else(|| GitError::not_found(format!("ref-delta base object {base_oid}")))?;
            let resolved = apply_pack_delta(&base.body, &body)?;
            Arc::new(EncodedObject::new(base.object_type, resolved))
        }
    };
    // Record the fully resolved object so any later read that walks through this
    // offset (as a delta base or directly) reuses it. Bases are inserted as the
    // recursion unwinds, so a chain is decoded at most once across reads.
    cache.insert(offset, Arc::clone(&object));
    Ok(object)
}

/// The object type and final (inflated) size of the entry at `offset`, *without*
/// materializing the object body — git's `cat-file --batch-check` fast path.
///
/// A base object's size is already in its pack entry header, and a delta's result
/// size is the second varint at the front of its (small) delta stream, so neither
/// inflates the full content. The reported type is the type at the end of the
/// delta chain (deltas inherit their base's type). `resolve_ref_base_type` supplies
/// the type of a ref-delta base that lives outside this pack (resolved through the
/// wider object store) and receives the cumulative depth after following that
/// ref-delta. `initial_delta_depth` carries depth already traversed in another
/// pack so mixed ref/ofs chains share the same finite recursion bound; top-level
/// callers pass zero. Ofs-delta bases are followed within `pack_bytes` directly.
pub fn read_object_header_at<F>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    initial_delta_depth: usize,
    mut resolve_ref_base_type: F,
) -> Result<PackObjectHeader>
where
    F: FnMut(&ObjectId, usize) -> Result<Option<PackObjectHeader>>,
{
    read_object_header_at_inner(
        pack_bytes,
        offset,
        format,
        initial_delta_depth,
        &mut resolve_ref_base_type,
        &mut NoopHeaderTypeCache,
    )
}

/// A header resolved through its complete delta chain without materializing the
/// object's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackObjectHeader {
    pub object_type: ObjectType,
    pub size: u64,
    /// Delta links below this entry (`0` for an undeltified object).
    pub delta_depth: usize,
}

impl PackObjectHeader {
    pub const fn undeltified(object_type: ObjectType, size: u64) -> Self {
        Self {
            object_type,
            size,
            delta_depth: 0,
        }
    }

    pub const fn type_and_size(self) -> (ObjectType, u64) {
        (self.object_type, self.size)
    }
}

/// Memo of `pack offset -> resolved header` for
/// the `cat-file --batch-check` header fast path.
///
/// Without it, resolving the *type* of an ofs-delta walks the whole delta chain
/// to its base on every header read, re-inflating each link's leading varints
/// from scratch — so reading every object in a deeply-deltified pack costs
/// O(objects x chain-depth) and goes super-linear (sley#26). Two reuses fall out
/// of memoizing `offset -> (type, size)`:
///
/// * a chain's end-of-chain type is resolved at most once, so later objects on
///   the same chain skip the walk; and
/// * a repeated lookup of the same object (common in batch input) returns from
///   the memo without re-inflating its delta header at all.
///
/// The size stored is the object's final (inflated) result size — read from its
/// own pack/delta header, never by materializing the body.
pub trait HeaderTypeCache {
    /// The previously resolved header at `pack_offset`, if any.
    fn get(&self, pack_offset: u64) -> Option<PackObjectHeader>;
    /// Record the resolved header at `pack_offset` for reuse by later reads.
    fn put(&mut self, pack_offset: u64, header: PackObjectHeader);
}

pub(crate) struct NoopHeaderTypeCache;

impl HeaderTypeCache for NoopHeaderTypeCache {
    fn get(&self, _pack_offset: u64) -> Option<PackObjectHeader> {
        None
    }
    fn put(&mut self, _pack_offset: u64, _header: PackObjectHeader) {}
}

/// Like [`read_object_header_at`] but threads a caller-owned [`HeaderTypeCache`]
/// through the read so (a) the ofs-delta chain's end-of-chain type is resolved at
/// most once per chain and (b) a repeated lookup of the same offset returns from
/// the memo without re-inflating (sley#26). The cache is keyed by in-pack offset,
/// so it must be scoped to a single pack's bytes by the caller. Depth semantics
/// match [`read_object_header_at`].
pub fn read_object_header_at_with_cache<F, C>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    initial_delta_depth: usize,
    mut resolve_ref_base_type: F,
    type_cache: &mut C,
) -> Result<PackObjectHeader>
where
    F: FnMut(&ObjectId, usize) -> Result<Option<PackObjectHeader>>,
    C: HeaderTypeCache + ?Sized,
{
    if let Some(header) = type_cache.get(offset) {
        checked_cached_header_depth(offset, initial_delta_depth, header.delta_depth)?;
        return Ok(header);
    }
    read_object_header_at_inner(
        pack_bytes,
        offset,
        format,
        initial_delta_depth,
        &mut resolve_ref_base_type,
        type_cache,
    )
}

pub(crate) fn read_object_header_at_inner<F, C>(
    pack_bytes: &[u8],
    offset: u64,
    format: ObjectFormat,
    delta_depth: usize,
    resolve_ref_base_type: &mut F,
    type_cache: &mut C,
) -> Result<PackObjectHeader>
where
    F: FnMut(&ObjectId, usize) -> Result<Option<PackObjectHeader>>,
    C: HeaderTypeCache + ?Sized,
{
    let trailer_offset = pack_bytes
        .len()
        .checked_sub(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("pack smaller than its trailer".into()))?;
    let entry_region = pack_entry_region(pack_bytes, trailer_offset)?;
    let mut cursor = usize::try_from(offset)
        .ok()
        .filter(|&value| value < trailer_offset)
        .ok_or_else(|| GitError::InvalidFormat("pack object offset out of range".into()))?;
    let header = parse_entry_header(entry_region, &mut cursor)?;
    let resolved = match header.kind {
        PackObjectKind::Commit => PackObjectHeader::undeltified(ObjectType::Commit, header.size),
        PackObjectKind::Tree => PackObjectHeader::undeltified(ObjectType::Tree, header.size),
        PackObjectKind::Blob => PackObjectHeader::undeltified(ObjectType::Blob, header.size),
        PackObjectKind::Tag => PackObjectHeader::undeltified(ObjectType::Tag, header.size),
        PackObjectKind::OfsDelta => {
            let next_delta_depth = checked_header_delta_depth(offset, delta_depth)?;
            let base_offset = parse_ofs_delta_base_offset(entry_region, &mut cursor, offset)?;
            let size = delta_result_size_from_stream(&entry_region[cursor..])?;
            // The end-of-chain type only depends on the base, so reuse it across
            // reads instead of re-walking the chain per object (sley#26).
            let base_header = match type_cache.get(base_offset) {
                Some(base_header) => {
                    checked_cached_header_depth(
                        base_offset,
                        next_delta_depth,
                        base_header.delta_depth,
                    )?;
                    base_header
                }
                None => read_object_header_at_inner(
                    pack_bytes,
                    base_offset,
                    format,
                    next_delta_depth,
                    resolve_ref_base_type,
                    type_cache,
                )?,
            };
            let resolved_delta_depth = checked_header_delta_depth(offset, base_header.delta_depth)?;
            PackObjectHeader {
                object_type: base_header.object_type,
                size,
                delta_depth: resolved_delta_depth,
            }
        }
        PackObjectKind::RefDelta => {
            let next_delta_depth = checked_header_delta_depth(offset, delta_depth)?;
            let hash_len = format.raw_len();
            if cursor + hash_len > trailer_offset {
                return Err(GitError::InvalidFormat(
                    "truncated ref-delta base object id".into(),
                ));
            }
            let oid = ObjectId::from_raw(format, &entry_region[cursor..cursor + hash_len])?;
            cursor += hash_len;
            let size = delta_result_size_from_stream(&entry_region[cursor..])?;
            let base_header = resolve_ref_base_type(&oid, next_delta_depth)?
                .ok_or_else(|| GitError::not_found(format!("ref-delta base object {oid}")))?;
            let resolved_delta_depth = checked_header_delta_depth(offset, base_header.delta_depth)?;
            checked_cached_header_depth(offset, delta_depth, resolved_delta_depth)?;
            PackObjectHeader {
                object_type: base_header.object_type,
                size,
                delta_depth: resolved_delta_depth,
            }
        }
    };
    // Memoize the fully resolved header so a repeated lookup of this offset (or a
    // chain that bases on it) returns without re-inflating (sley#26).
    type_cache.put(offset, resolved);
    Ok(resolved)
}

fn checked_header_delta_depth(offset: u64, delta_depth: usize) -> Result<usize> {
    let observed_depth = delta_depth.checked_add(1).ok_or_else(|| {
        GitError::InvalidFormat(format!(
            "pack delta chain depth overflows at offset {offset}"
        ))
    })?;
    if observed_depth > MAX_READ_DELTA_CHAIN_DEPTH {
        return Err(GitError::InvalidFormat(format!(
            "pack delta chain at offset {offset} has observed depth {observed_depth}, which \
             exceeds maximum depth (configured limit {MAX_READ_DELTA_CHAIN_DEPTH})"
        )));
    }
    Ok(observed_depth)
}

fn checked_cached_header_depth(
    offset: u64,
    initial_delta_depth: usize,
    cached_delta_depth: usize,
) -> Result<()> {
    let observed_depth = initial_delta_depth
        .checked_add(cached_delta_depth)
        .ok_or_else(|| {
            GitError::InvalidFormat(format!(
                "pack delta chain depth overflows at offset {offset}"
            ))
        })?;
    if observed_depth > MAX_READ_DELTA_CHAIN_DEPTH {
        return Err(GitError::InvalidFormat(format!(
            "pack delta chain at offset {offset} has observed depth {observed_depth}, which \
             exceeds maximum depth (configured limit {MAX_READ_DELTA_CHAIN_DEPTH})"
        )));
    }
    Ok(())
}

/// Number of inflated delta-stream bytes to read when only the leading base-size
/// and result-size varints are needed. Each varint is at most 10 bytes, so a short
/// prefix always covers both without inflating the delta instructions.
pub(crate) const DELTA_HEADER_PREFIX_LEN: usize = 32;

/// Result size of a delta whose zlib-compressed stream starts at `compressed`,
/// inflating only the short prefix that holds its two leading varints.
pub(crate) fn delta_result_size_from_stream(compressed: &[u8]) -> Result<u64> {
    let mut prefix = [0u8; DELTA_HEADER_PREFIX_LEN];
    let written = inflate_prefix(compressed, &mut prefix)?;
    decoded_delta_result_size(&prefix[..written])
}

/// The pack's entry region: everything between the 12-byte header and the
/// trailing checksum.
///
/// Every varint cursor must walk *this* slice rather than the whole pack.
/// [`next_byte`] stops at the end of whatever slice it is handed, so passing it
/// the pack *including* the trailer lets an entry header whose continuation bit
/// never clears go on consuming checksum bytes, leaving the cursor past
/// `trailer_offset`. The entry-body slice `[cursor..trailer_offset]` that
/// follows is then built with `start > end`, which panics — on remote input,
/// since packs arrive straight off the wire (sley#162).
///
/// Bounding the cursor here makes that unrepresentable: a runaway varint simply
/// runs out of slice and is reported as the truncated header it always was.
fn pack_entry_region(bytes: &[u8], trailer_offset: usize) -> Result<&[u8]> {
    bytes
        .get(..trailer_offset)
        .ok_or_else(|| GitError::InvalidFormat("pack smaller than its trailer".into()))
}

pub(crate) fn parse_entry_header(bytes: &[u8], offset: &mut usize) -> Result<EntryHeader> {
    let first = next_byte(bytes, offset)?;
    let mut size = u64::from(first & 0x0f);
    let kind = match (first >> 4) & 0x07 {
        1 => PackObjectKind::Commit,
        2 => PackObjectKind::Tree,
        3 => PackObjectKind::Blob,
        4 => PackObjectKind::Tag,
        6 => PackObjectKind::OfsDelta,
        7 => PackObjectKind::RefDelta,
        other => {
            return Err(GitError::InvalidFormat(format!(
                "invalid pack object type {other}"
            )));
        }
    };
    let mut shift = 4;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = next_byte(bytes, offset)?;
        let part = u64::from(byte & 0x7f);
        size = size
            .checked_add(
                part.checked_shl(shift)
                    .ok_or_else(|| GitError::InvalidFormat("pack size overflow".into()))?,
            )
            .ok_or_else(|| GitError::InvalidFormat("pack size overflow".into()))?;
        shift += 7;
    }
    Ok(EntryHeader { kind, size })
}

pub(crate) fn parse_ofs_delta_base_offset(
    bytes: &[u8],
    offset: &mut usize,
    entry_offset: u64,
) -> Result<u64> {
    let mut byte = next_byte(bytes, offset)?;
    let mut relative = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = next_byte(bytes, offset)?;
        relative = relative
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(u64::from(byte & 0x7f)))
            .ok_or_else(|| GitError::InvalidFormat("ofs-delta offset overflow".into()))?;
    }
    entry_offset
        .checked_sub(relative)
        .ok_or_else(|| GitError::InvalidFormat("ofs-delta points before pack start".into()))
}

pub(crate) fn resolve_pack_entries<F>(
    parsed: Vec<ParsedPackEntry>,
    format: ObjectFormat,
    external_base: &mut F,
    limits: PackReadLimits,
) -> Result<Vec<PackObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    let mut offset_to_index = HashMap::with_capacity(parsed.len());
    for (idx, entry) in parsed.iter().enumerate() {
        offset_to_index.insert(parsed_entry_offset(entry), idx);
    }

    let mut resolved = vec![None; parsed.len()];
    // sley#5: chain depth of each resolved entry. Undeltified entries and
    // entries resolved against an external (thin-pack) base are depth 0; a
    // delta is one deeper than the base it was applied to.
    let mut depths = vec![0usize; parsed.len()];
    let mut oid_to_index = HashMap::new();
    let mut unresolved = 0usize;
    for (idx, entry) in parsed.iter().enumerate() {
        match entry {
            ParsedPackEntry::Resolved(object) => {
                oid_to_index.insert(object.entry.oid, idx);
                resolved[idx] = Some(object.clone());
            }
            ParsedPackEntry::Delta { .. } => unresolved += 1,
        }
    }

    while unresolved != 0 {
        let mut progress = false;
        for idx in 0..parsed.len() {
            if resolved[idx].is_some() {
                continue;
            }
            let ParsedPackEntry::Delta {
                base,
                compressed_size,
                delta_size,
                offset,
                delta,
            } = &parsed[idx]
            else {
                continue;
            };
            let Some(base_object) = delta_base_object(
                base,
                &offset_to_index,
                &oid_to_index,
                &resolved,
                external_base,
            )?
            else {
                continue;
            };
            // sley#5: reject before applying the delta, so an over-deep chain
            // costs nothing beyond the walk that discovered it. An external
            // base is depth 0 because its own chain lives in another pack that
            // was bounded when it was read.
            let base_depth = match base {
                DeltaBase::Offset(base_offset) => {
                    offset_to_index.get(base_offset).map(|idx| depths[*idx])
                }
                DeltaBase::Ref(base_oid) => oid_to_index.get(base_oid).map(|idx| depths[*idx]),
            }
            .unwrap_or(0);
            let depth = base_depth + 1;
            if depth > limits.max_delta_depth {
                return Err(GitError::InvalidFormat(format!(
                    "pack delta chain at offset {offset} has observed depth {depth}, which \
                     exceeds maximum depth (configured limit {}); raise \
                     PackReadLimits::max_delta_depth or run `git repack --depth={}`",
                    limits.max_delta_depth, limits.max_delta_depth
                )));
            }
            let body = apply_pack_delta(base_object.body(), delta)?;
            let object = EncodedObject::new(base_object.object_type(), body);
            #[cfg(feature = "fetch-profile")]
            let _oid_span =
                sley_core::fetch_profile::Span::enter(sley_core::fetch_profile::Stage::OidHash);
            let oid = object.object_id(format)?;
            #[cfg(feature = "fetch-profile")]
            {
                sley_core::fetch_profile::add_count(sley_core::fetch_profile::Stage::OidHash, 1);
                sley_core::fetch_profile::add_bytes(
                    sley_core::fetch_profile::Stage::OidHash,
                    object.body.len() as u64,
                );
            }
            let pack_object = PackObject {
                entry: PackEntry {
                    oid,
                    compressed_size: *compressed_size,
                    uncompressed_size: object.body.len() as u64,
                    offset: *offset,
                },
                object,
            };
            if pack_object.entry.uncompressed_size != decoded_delta_result_size(delta)? {
                return Err(GitError::InvalidObject(
                    "resolved delta size does not match delta header".into(),
                ));
            }
            if *delta_size != delta.len() as u64 {
                return Err(GitError::InvalidObject(format!(
                    "pack delta declared {delta_size} bytes, decoded {}",
                    delta.len()
                )));
            }
            oid_to_index.insert(oid, idx);
            depths[idx] = depth;
            resolved[idx] = Some(pack_object);
            unresolved -= 1;
            progress = true;
        }
        if !progress {
            return Err(GitError::Unsupported("unresolved delta base".into()));
        }
    }

    resolved
        .into_iter()
        .map(|entry| entry.ok_or_else(|| GitError::InvalidFormat("unresolved pack entry".into())))
        .collect()
}

pub(crate) fn parsed_entry_offset(entry: &ParsedPackEntry) -> u64 {
    match entry {
        ParsedPackEntry::Resolved(object) => object.entry.offset,
        ParsedPackEntry::Delta { offset, .. } => *offset,
    }
}

pub(crate) enum DeltaBaseObject<'a> {
    Borrowed(&'a EncodedObject),
    Owned(EncodedObject),
}

impl DeltaBaseObject<'_> {
    pub(crate) fn object_type(&self) -> ObjectType {
        match self {
            Self::Borrowed(object) => object.object_type,
            Self::Owned(object) => object.object_type,
        }
    }

    pub(crate) fn body(&self) -> &[u8] {
        match self {
            Self::Borrowed(object) => &object.body,
            Self::Owned(object) => &object.body,
        }
    }
}

pub(crate) fn delta_base_object<'a, F>(
    base: &DeltaBase,
    offset_to_index: &HashMap<u64, usize>,
    oid_to_index: &HashMap<ObjectId, usize>,
    resolved: &'a [Option<PackObject>],
    external_base: &mut F,
) -> Result<Option<DeltaBaseObject<'a>>>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    match base {
        DeltaBase::Offset(offset) => {
            let Some(index) = offset_to_index.get(offset).copied() else {
                return Err(GitError::InvalidFormat(format!(
                    "ofs-delta base offset {offset} not found"
                )));
            };
            Ok(resolved[index]
                .as_ref()
                .map(|object| DeltaBaseObject::Borrowed(&object.object)))
        }
        DeltaBase::Ref(oid) => {
            if let Some(index) = oid_to_index.get(oid).copied() {
                return Ok(resolved[index]
                    .as_ref()
                    .map(|object| DeltaBaseObject::Borrowed(&object.object)));
            }
            external_base(oid).map(|object| object.map(DeltaBaseObject::Owned))
        }
    }
}

pub(crate) fn apply_pack_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let plan = plan_pack_delta(base, delta)?;
    let result_size = plan.result_size;
    let result_size_hint = usize::try_from(result_size).unwrap_or(usize::MAX);
    // Preserve the legacy decoder's bounded speculative reservation followed by
    // geometric Vec growth. Its malformed-input bytes, errors, and complexity
    // are part of the Git-parity surface.
    let mut result = Vec::with_capacity(inflate::bounded_inflate_reserve(
        result_size_hint,
        delta.len(),
    ));
    walk_pack_delta(base, delta, plan, CancelFlag::never(), |slice| {
        result.extend_from_slice(slice);
        Ok(())
    })?;
    if result.len() as u64 != result_size {
        return Err(GitError::InvalidObject(format!(
            "delta result size mismatch: expected {result_size}, got {}",
            result.len()
        )));
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PackDeltaPlan {
    instructions_offset: usize,
    pub(crate) result_size: u64,
}

pub(crate) fn plan_pack_delta(base: &[u8], delta: &[u8]) -> Result<PackDeltaPlan> {
    let mut instructions_offset = 0usize;
    let base_size = read_delta_varint(delta, &mut instructions_offset)?;
    if base_size != base.len() as u64 {
        return Err(GitError::InvalidObject(format!(
            "delta base size mismatch: expected {base_size}, got {}",
            base.len()
        )));
    }
    let result_size = read_delta_varint(delta, &mut instructions_offset)?;
    Ok(PackDeltaPlan {
        instructions_offset,
        result_size,
    })
}

pub(crate) fn apply_pack_delta_exact(
    base: &[u8],
    delta: &[u8],
    plan: PackDeltaPlan,
    result: &mut Vec<u8>,
    cancel: CancelFlag<'_>,
) -> Result<()> {
    if !result.is_empty() || u64::try_from(result.capacity()).unwrap_or(u64::MAX) < plan.result_size
    {
        return Err(GitError::InvalidObject(
            "delta output buffer is not empty and preallocated to the declared result size".into(),
        ));
    }
    walk_pack_delta(base, delta, plan, cancel, |slice| {
        let end = result
            .len()
            .checked_add(slice.len())
            .ok_or_else(|| GitError::InvalidObject("delta output range overflow".into()))?;
        if u64::try_from(end).unwrap_or(u64::MAX) > plan.result_size {
            return Err(GitError::InvalidObject(
                "delta instructions exceed declared result size".into(),
            ));
        }
        result.extend_from_slice(slice);
        Ok(())
    })?;
    cancel.check()?;
    if result.len() as u64 != plan.result_size {
        return Err(GitError::InvalidObject(format!(
            "delta result size mismatch: expected {}, got {}",
            plan.result_size,
            result.len()
        )));
    }
    Ok(())
}

fn walk_pack_delta<F>(
    base: &[u8],
    delta: &[u8],
    plan: PackDeltaPlan,
    cancel: CancelFlag<'_>,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    let mut cursor = plan.instructions_offset;
    while cursor < delta.len() {
        cancel.check()?;
        let command = delta[cursor];
        cursor += 1;
        if command & 0x80 != 0 {
            let copy_offset =
                read_delta_copy_value(delta, &mut cursor, command, &[0x01, 0x02, 0x04, 0x08])?;
            let mut copy_size =
                read_delta_copy_value(delta, &mut cursor, command, &[0x10, 0x20, 0x40])?;
            if copy_size == 0 {
                copy_size = 0x10000;
            }
            let start = usize::try_from(copy_offset)
                .map_err(|_| GitError::InvalidObject("delta copy offset overflows usize".into()))?;
            let len = usize::try_from(copy_size)
                .map_err(|_| GitError::InvalidObject("delta copy size overflows usize".into()))?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| GitError::InvalidObject("delta copy range overflow".into()))?;
            let Some(slice) = base.get(start..end) else {
                return Err(GitError::InvalidObject(
                    "delta copy range exceeds base object".into(),
                ));
            };
            emit(slice)?;
        } else if command != 0 {
            let len = usize::from(command);
            let end = cursor
                .checked_add(len)
                .ok_or_else(|| GitError::InvalidObject("delta insert range overflow".into()))?;
            let Some(slice) = delta.get(cursor..end) else {
                return Err(GitError::InvalidObject(
                    "delta insert range exceeds delta data".into(),
                ));
            };
            emit(slice)?;
            cursor = end;
        } else {
            return Err(GitError::InvalidObject(
                "delta contains reserved zero command".into(),
            ));
        }
    }
    cancel.check()?;
    Ok(())
}

pub(crate) fn decoded_delta_result_size(delta: &[u8]) -> Result<u64> {
    let mut cursor = 0usize;
    let _ = read_delta_varint(delta, &mut cursor)?;
    read_delta_varint(delta, &mut cursor)
}

pub(crate) fn read_delta_varint(delta: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let Some(byte) = delta.get(*cursor).copied() else {
            return Err(GitError::InvalidObject("truncated delta size".into()));
        };
        *cursor += 1;
        value = value
            .checked_add(
                u64::from(byte & 0x7f)
                    .checked_shl(shift)
                    .ok_or_else(|| GitError::InvalidObject("delta size overflow".into()))?,
            )
            .ok_or_else(|| GitError::InvalidObject("delta size overflow".into()))?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .ok_or_else(|| GitError::InvalidObject("delta size overflow".into()))?;
    }
}

pub(crate) fn read_delta_copy_value(
    delta: &[u8],
    cursor: &mut usize,
    command: u8,
    masks: &[u8],
) -> Result<u64> {
    let mut value = 0u64;
    for (shift, mask) in masks.iter().enumerate() {
        if command & mask != 0 {
            let Some(byte) = delta.get(*cursor).copied() else {
                return Err(GitError::InvalidObject(
                    "truncated delta copy command".into(),
                ));
            };
            *cursor += 1;
            value |= u64::from(byte) << (shift * 8);
        }
    }
    Ok(value)
}
