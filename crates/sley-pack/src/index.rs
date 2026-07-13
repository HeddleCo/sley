//! Pack index, reverse index, mtimes, bitmap, multi-pack-index, and EWAH helpers.
//!
//! Split out of `lib.rs` in the W21 mechanical refactor: a pure code move
//! (no function body changed); all items are re-exported from `lib.rs`.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexBuild {
    pub index: Vec<u8>,
    pub pack_checksum: ObjectId,
    pub entries: Vec<PackIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStreamIndexBuild {
    pub index: Vec<u8>,
    pub pack_checksum: ObjectId,
    pub entries: Vec<PackIndexEntry>,
    pub objects: Vec<PackIndexedObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexedObject {
    pub oid: ObjectId,
    pub object_type: ObjectType,
    pub size: u64,
    pub offset: u64,
}

/// Streaming pack-receive counters emitted while `index_pack_from_stream`
/// parses a pack off a reader. All fields are monotonically non-decreasing
/// within one indexing pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackStreamProgress {
    /// Pack bytes consumed from the reader so far (header + object entries),
    /// i.e. the streaming pack offset. Advances during download for streaming
    /// transports; during the index walk for already-buffered ones.
    pub received_bytes: u64,
    /// Objects parsed from the pack so far.
    pub received_objects: u64,
    /// Total objects declared by the pack header (known once the 12-byte header
    /// is read).
    pub total_objects: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndex {
    pub version: u32,
    pub fanout: [u32; 256],
    pub entries: Vec<PackIndexEntry>,
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexView<'a> {
    pub version: u32,
    pub count: usize,
    pub fanout: [u32; 256],
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
    bytes: &'a [u8],
    format: ObjectFormat,
    tables: PackIndexViewTables,
}

pub trait PackIndexByteSource: fmt::Debug + Send + Sync {
    fn as_bytes(&self) -> &[u8];
}

impl<T> PackIndexByteSource for T
where
    T: AsRef<[u8]> + fmt::Debug + Send + Sync + ?Sized,
{
    fn as_bytes(&self) -> &[u8] {
        self.as_ref()
    }
}

#[derive(Debug)]
pub(crate) struct SharedIndexBytes(Arc<[u8]>);

impl PackIndexByteSource for SharedIndexBytes {
    fn as_bytes(&self) -> &[u8] {
        self.0.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct PackIndexViewData {
    pub version: u32,
    pub count: usize,
    pub fanout: [u32; 256],
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
    bytes: Arc<dyn PackIndexByteSource>,
    format: ObjectFormat,
    tables: PackIndexViewTables,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackIndexEntry {
    pub oid: ObjectId,
    pub crc32: u32,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackIndexLookup {
    pub crc32: u32,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackIndexViewTables {
    V1 {
        entry_table: Range<usize>,
    },
    V2 {
        oid_table: Range<usize>,
        crc_table: Range<usize>,
        small_offset_table: Range<usize>,
        large_offset_table: Range<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackReverseIndex {
    pub version: u32,
    pub format: ObjectFormat,
    pub positions: Vec<u32>,
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackMtimes {
    pub version: u32,
    pub format: ObjectFormat,
    pub mtimes: Vec<u32>,
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBitmapIndex {
    pub version: u16,
    pub format: ObjectFormat,
    pub options: u16,
    pub pack_checksum: ObjectId,
    pub index_checksum: ObjectId,
    pub type_bitmaps: PackBitmapTypeBitmaps,
    pub entries: Vec<PackBitmapEntry>,
    pub pseudo_merges: Vec<PackBitmapPseudoMerge>,
    /// Whether the serialised bitmap carries the commit lookup-table
    /// extension. The table itself is derived deterministically from entries.
    pub lookup_table: bool,
    pub name_hash_cache: Option<Vec<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBitmapTypeBitmaps {
    pub commits: EwahBitmap,
    pub trees: EwahBitmap,
    pub blobs: EwahBitmap,
    pub tags: EwahBitmap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBitmapEntry {
    /// The commit's position in the *oid-sorted* pack index (`.idx` order),
    /// NOT the pack-order position used for the bitmap's bit numbering.
    /// Upstream writes `oid_pos(...)` here (pack-bitmap-write.c) and reads it
    /// back via `nth_packed_object_id` (pack-bitmap.c).
    pub object_position: u32,
    pub xor_offset: u8,
    pub flags: u8,
    /// Reachability bitmap; bit `i` refers to the `i`-th object in *pack
    /// order* (offset order), as mapped by the pack's reverse index.
    pub bitmap: EwahBitmap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackBitmapPseudoMerge {
    /// Commit bits, in the bitmap's bit-numbering order, covered by this
    /// pseudo-merge.
    pub commits: EwahBitmap,
    /// Object reachability closure for the pseudo-merge's commits, in the same
    /// bit-numbering order.
    pub bitmap: EwahBitmap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EwahBitmap {
    pub bit_size: u32,
    pub words: Vec<u64>,
    pub rlw_position: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPackIndex {
    pub version: u8,
    pub format: ObjectFormat,
    pub pack_count: u32,
    pub pack_names: Vec<String>,
    pub object_count: u32,
    pub fanout: [u32; 256],
    pub objects: Vec<MultiPackIndexEntry>,
    pub reverse_index: Option<Vec<u32>>,
    pub bitmapped_packs: Option<Vec<MultiPackBitmapPack>>,
    pub chunks: Vec<MultiPackIndexChunk>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone)]
pub struct MultiPackIndexOidLookup {
    format: ObjectFormat,
    pack_count: u32,
    pack_names: Vec<String>,
    fanout: [u32; 256],
    object_count: usize,
    oid_lookup_offset: usize,
    object_offsets_offset: usize,
    large_offsets_offset: Option<usize>,
    large_offsets_len: usize,
    bytes: Arc<dyn PackIndexByteSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPackIndexEntry {
    pub oid: ObjectId,
    pub pack_int_id: u32,
    pub offset: u64,
    pub force_large_offset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPackBitmapPack {
    pub bitmap_pos: u32,
    pub bitmap_nr: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPackIndexChunk {
    pub id: [u8; 4],
    pub offset: u64,
    pub len: u64,
}
impl<'a> PackIndexView<'a> {
    pub fn parse_v2_sha1(bytes: &'a [u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn parse(bytes: &'a [u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, true, true)
    }

    /// Parse and validate the index layout without recomputing the trailing
    /// index checksum. The checksum stored in the file is still exposed via
    /// [`PackIndexView::index_checksum`].
    pub fn parse_without_checksum(bytes: &'a [u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, false, true)
    }

    /// Parse a local/trusted pack index without recomputing the trailing index
    /// checksum or walking every entry for canonical-order validation.
    ///
    /// This still validates the table layout and all lookup paths remain
    /// bounds-checked, but it avoids O(number-of-objects) startup validation for
    /// repository-owned `.idx` files in hot read paths.
    pub fn parse_trusted_without_checksum(bytes: &'a [u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, false, false)
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn fanout(&self) -> &[u32; 256] {
        &self.fanout
    }

    pub fn find(&self, oid: &ObjectId) -> Option<PackIndexLookup> {
        if oid.format() != self.format {
            return None;
        }
        let bucket = usize::from(oid.as_bytes()[0]);
        let mut start = if bucket == 0 {
            0
        } else {
            self.fanout[bucket - 1] as usize
        };
        let mut end = self.fanout[bucket] as usize;
        let target = oid.as_bytes();

        while start < end {
            let mid = start + (end - start) / 2;
            match self.oid_bytes_at(mid).cmp(target) {
                std::cmp::Ordering::Less => start = mid + 1,
                std::cmp::Ordering::Equal => return self.lookup_at(mid),
                std::cmp::Ordering::Greater => end = mid,
            }
        }
        None
    }

    pub(crate) fn parse_impl(
        bytes: &'a [u8],
        format: ObjectFormat,
        verify_checksum: bool,
        validate_entries: bool,
    ) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        if bytes[..4] != [0xff, b't', b'O', b'c'] {
            return Self::parse_v1_impl(bytes, format, verify_checksum, validate_entries);
        }
        if bytes.len() < 8 + 256 * 4 + 2 * hash_len {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let index_checksum_offset = bytes.len() - hash_len;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if verify_checksum {
            let actual_index_checksum =
                sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
            if actual_index_checksum != index_checksum {
                return Err(GitError::InvalidFormat(format!(
                    "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
                )));
            }
        }

        let mut offset = 8usize;
        let fanout = read_pack_index_fanout(bytes, &mut offset)?;
        let count = fanout[255] as usize;
        let oid_table = checked_range(offset, count, hash_len, bytes.len())?;
        offset = oid_table.end;
        let crc_table = checked_range(offset, count, 4, bytes.len())?;
        offset = crc_table.end;
        let small_offset_table = checked_range(offset, count, 4, bytes.len())?;
        offset = small_offset_table.end;

        let large_offset_count = (0..count)
            .filter(|idx| {
                let start = small_offset_table.start + idx * 4;
                u32_be(&bytes[start..start + 4]) & 0x8000_0000 != 0
            })
            .count();
        let mut large_offset_table = checked_range(offset, large_offset_count, 8, bytes.len())?;
        offset = large_offset_table.end;

        let expected_trailer_offset = bytes.len() - hash_len * 2;
        if offset != expected_trailer_offset {
            if !verify_checksum && offset < expected_trailer_offset {
                large_offset_table = large_offset_table.start..expected_trailer_offset;
                offset = expected_trailer_offset;
            } else {
                return Err(GitError::InvalidFormat(format!(
                    "pack index has {} unexpected bytes before trailer",
                    expected_trailer_offset.saturating_sub(offset)
                )));
            }
        }
        let pack_checksum = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;

        let view = Self {
            version,
            count,
            fanout,
            pack_checksum,
            index_checksum,
            bytes,
            format,
            tables: PackIndexViewTables::V2 {
                oid_table,
                crc_table,
                small_offset_table,
                large_offset_table,
            },
        };
        if validate_entries {
            view.validate_v2_entries()?;
        }
        Ok(view)
    }

    pub(crate) fn parse_v1_impl(
        bytes: &'a [u8],
        format: ObjectFormat,
        verify_checksum: bool,
        validate_entries: bool,
    ) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 256 * 4 + 2 * hash_len {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let index_checksum_offset = bytes.len() - hash_len;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if verify_checksum {
            let actual_index_checksum =
                sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
            if actual_index_checksum != index_checksum {
                return Err(GitError::InvalidFormat(format!(
                    "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
                )));
            }
        }

        let mut offset = 0usize;
        let fanout = read_pack_index_fanout(bytes, &mut offset)?;
        let count = fanout[255] as usize;
        let entry_len = hash_len
            .checked_add(4)
            .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
        let entry_table = checked_range(offset, count, entry_len, bytes.len())?;
        offset = entry_table.end;
        let expected_trailer_offset = bytes.len() - hash_len * 2;
        if offset != expected_trailer_offset {
            return Err(GitError::InvalidFormat(format!(
                "pack index has {} unexpected bytes before trailer",
                expected_trailer_offset.saturating_sub(offset)
            )));
        }
        let pack_checksum = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;

        let view = Self {
            version: 1,
            count,
            fanout,
            pack_checksum,
            index_checksum,
            bytes,
            format,
            tables: PackIndexViewTables::V1 { entry_table },
        };
        if validate_entries {
            view.validate_v1_entries()?;
        }
        Ok(view)
    }

    pub(crate) fn validate_v2_entries(&self) -> Result<()> {
        let PackIndexViewTables::V2 {
            oid_table,
            small_offset_table,
            large_offset_table,
            ..
        } = &self.tables
        else {
            unreachable!("v2 validation only runs for v2 views");
        };
        let oid_table = self.slice(oid_table.clone());
        let small_offset_table = self.slice(small_offset_table.clone());
        let large_offset_table = self.slice(large_offset_table.clone());
        let hash_len = self.format.raw_len();
        for idx in 0..self.count {
            let oid_start = idx * hash_len;
            let oid_bytes = &oid_table[oid_start..oid_start + hash_len];
            if idx > 0 && oid_bytes < &oid_table[oid_start - hash_len..oid_start] {
                return Err(GitError::InvalidFormat(
                    "pack index object ids are not sorted".into(),
                ));
            }
            validate_pack_index_oid_fanout(idx, oid_bytes, &self.fanout)?;

            let offset_start = idx * 4;
            let raw_offset = u32_be(&small_offset_table[offset_start..offset_start + 4]);
            pack_index_v2_offset(raw_offset, large_offset_table)?;
        }
        Ok(())
    }

    pub(crate) fn validate_v1_entries(&self) -> Result<()> {
        let PackIndexViewTables::V1 { entry_table } = &self.tables else {
            unreachable!("v1 validation only runs for v1 views");
        };
        let entry_table = self.slice(entry_table.clone());
        let hash_len = self.format.raw_len();
        let entry_len = hash_len
            .checked_add(4)
            .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
        for idx in 0..self.count {
            let start = idx * entry_len;
            let oid_start = start + 4;
            let oid_bytes = &entry_table[oid_start..start + entry_len];
            if idx > 0 {
                let previous_oid_start = oid_start - entry_len;
                let previous_oid = &entry_table[previous_oid_start..previous_oid_start + hash_len];
                if previous_oid > oid_bytes {
                    return Err(GitError::InvalidFormat(
                        "pack index object ids are not sorted".into(),
                    ));
                }
            }
            validate_pack_index_oid_fanout(idx, oid_bytes, &self.fanout)?;
        }
        Ok(())
    }

    pub(crate) fn oid_bytes_at(&self, idx: usize) -> &'a [u8] {
        let hash_len = self.format.raw_len();
        match &self.tables {
            PackIndexViewTables::V1 { entry_table } => {
                let entry_table = self.slice(entry_table.clone());
                let entry_len = hash_len + 4;
                let start = idx * entry_len + 4;
                &entry_table[start..start + hash_len]
            }
            PackIndexViewTables::V2 { oid_table, .. } => {
                let oid_table = self.slice(oid_table.clone());
                let start = idx * hash_len;
                &oid_table[start..start + hash_len]
            }
        }
    }

    pub(crate) fn lookup_at(&self, idx: usize) -> Option<PackIndexLookup> {
        if idx >= self.count {
            return None;
        }
        let hash_len = self.format.raw_len();
        match &self.tables {
            PackIndexViewTables::V1 { entry_table } => {
                let entry_table = self.slice(entry_table.clone());
                let entry_len = hash_len + 4;
                let start = idx * entry_len;
                Some(PackIndexLookup {
                    crc32: 0,
                    offset: u64::from(u32_be(&entry_table[start..start + 4])),
                })
            }
            PackIndexViewTables::V2 {
                crc_table,
                small_offset_table,
                large_offset_table,
                ..
            } => {
                let crc_table = self.slice(crc_table.clone());
                let small_offset_table = self.slice(small_offset_table.clone());
                let large_offset_table = self.slice(large_offset_table.clone());
                let crc_start = idx * 4;
                let raw_offset = u32_be(&small_offset_table[crc_start..crc_start + 4]);
                Some(PackIndexLookup {
                    crc32: u32_be(&crc_table[crc_start..crc_start + 4]),
                    offset: pack_index_v2_offset(raw_offset, large_offset_table).ok()?,
                })
            }
        }
    }

    pub(crate) fn slice(&self, range: Range<usize>) -> &'a [u8] {
        &self.bytes[range]
    }
}

impl PackIndexViewData {
    pub fn parse(bytes: Arc<[u8]>, format: ObjectFormat) -> Result<Self> {
        Self::parse_source(Arc::new(SharedIndexBytes(bytes)), format)
    }

    /// Parse and validate an owned index view without recomputing the trailing
    /// index checksum. The stored checksum is still exposed via
    /// [`PackIndexViewData::index_checksum`].
    pub fn parse_without_checksum(bytes: Arc<[u8]>, format: ObjectFormat) -> Result<Self> {
        Self::parse_source_without_checksum(Arc::new(SharedIndexBytes(bytes)), format)
    }

    /// Parse a local/trusted owned index view without the checksum or full-entry
    /// validation passes.
    pub fn parse_trusted_without_checksum(bytes: Arc<[u8]>, format: ObjectFormat) -> Result<Self> {
        Self::parse_trusted_source_without_checksum(Arc::new(SharedIndexBytes(bytes)), format)
    }

    pub fn parse_source(bytes: Arc<dyn PackIndexByteSource>, format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, true, true)
    }

    pub fn parse_source_without_checksum(
        bytes: Arc<dyn PackIndexByteSource>,
        format: ObjectFormat,
    ) -> Result<Self> {
        Self::parse_impl(bytes, format, false, true)
    }

    pub fn parse_trusted_source_without_checksum(
        bytes: Arc<dyn PackIndexByteSource>,
        format: ObjectFormat,
    ) -> Result<Self> {
        Self::parse_impl(bytes, format, false, false)
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn fanout(&self) -> &[u32; 256] {
        &self.fanout
    }

    pub fn find(&self, oid: &ObjectId) -> Option<PackIndexLookup> {
        self.as_view().find(oid)
    }

    pub fn as_view(&self) -> PackIndexView<'_> {
        PackIndexView {
            version: self.version,
            count: self.count,
            fanout: self.fanout,
            pack_checksum: self.pack_checksum,
            index_checksum: self.index_checksum,
            bytes: self.bytes.as_bytes(),
            format: self.format,
            tables: self.tables.clone(),
        }
    }

    /// Offset/CRC lookup for the entry at `idx` in oid-sorted pack-index order.
    pub fn lookup_at(&self, idx: usize) -> Option<PackIndexLookup> {
        self.as_view().lookup_at(idx)
    }

    /// The object id at `idx` in oid-sorted pack-index order.
    pub fn oid_at(&self, idx: usize) -> Result<ObjectId> {
        if idx >= self.count {
            return Err(GitError::InvalidFormat(
                "pack index position out of range".into(),
            ));
        }
        ObjectId::from_raw(self.format, self.as_view().oid_bytes_at(idx))
    }

    /// Resolve a pack offset to its object id by scanning every index entry.
    pub fn oid_at_offset_linear(&self, offset: u64) -> Option<ObjectId> {
        let view = self.as_view();
        for idx in 0..self.count {
            let lookup = view.lookup_at(idx)?;
            if lookup.offset == offset {
                return self.oid_at(idx).ok();
            }
        }
        None
    }

    pub(crate) fn parse_impl(
        bytes: Arc<dyn PackIndexByteSource>,
        format: ObjectFormat,
        verify_checksum: bool,
        validate_entries: bool,
    ) -> Result<Self> {
        let (version, count, fanout, pack_checksum, index_checksum, tables) = {
            let view = PackIndexView::parse_impl(
                bytes.as_bytes(),
                format,
                verify_checksum,
                validate_entries,
            )?;
            (
                view.version,
                view.count,
                view.fanout,
                view.pack_checksum,
                view.index_checksum,
                view.tables,
            )
        };
        Ok(Self {
            version,
            count,
            fanout,
            pack_checksum,
            index_checksum,
            bytes,
            format,
            tables,
        })
    }
}

impl PackIndex {
    pub fn write_v2_for_pack_sha1(pack_bytes: &[u8]) -> Result<PackIndexBuild> {
        Self::write_v2_for_pack(pack_bytes, ObjectFormat::Sha1)
    }

    pub fn write_v2_for_pack(pack_bytes: &[u8], format: ObjectFormat) -> Result<PackIndexBuild> {
        Self::write_v2_for_pack_with_base(pack_bytes, format, |_| Ok(None))
    }

    /// Validate and index a pack while resolving ref-deltas against an external
    /// object source. This powers `index-pack --fix-thin`; self-contained packs
    /// use [`Self::write_v2_for_pack`].
    pub fn write_v2_for_pack_with_base<F>(
        pack_bytes: &[u8],
        format: ObjectFormat,
        mut external_base: F,
    ) -> Result<PackIndexBuild>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        let trailer_len = format.raw_len();
        if pack_bytes.len() < 12 + trailer_len {
            return Err(GitError::InvalidFormat("pack file too short".into()));
        }
        let trailer_offset = pack_bytes.len() - trailer_len;
        let pack_checksum = sley_core::digest_bytes(format, &pack_bytes[..trailer_offset])?;
        let expected = ObjectId::from_raw(format, &pack_bytes[trailer_offset..])?;
        if pack_checksum != expected {
            return Err(GitError::InvalidFormat(format!(
                "pack checksum mismatch: expected {expected}, got {pack_checksum}"
            )));
        }

        if &pack_bytes[..4] != b"PACK" {
            return Err(GitError::InvalidFormat("missing PACK signature".into()));
        }
        let version = u32_be(&pack_bytes[4..8]);
        if version != 2 && version != 3 {
            return Err(GitError::Unsupported(format!("pack version {version}")));
        }
        let count = u32_be(&pack_bytes[8..12]) as usize;
        let mut offset = 12usize;
        let mut parsed_entries = Vec::with_capacity(count);
        let mut raw_entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_offset = offset;
            let header = parse_entry_header(pack_bytes, &mut offset)?;
            let base = match header.kind {
                PackObjectKind::OfsDelta => Some(DeltaBase::Offset(parse_ofs_delta_base_offset(
                    pack_bytes,
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
                    let oid = ObjectId::from_raw(format, &pack_bytes[offset..offset + hash_len])?;
                    offset += hash_len;
                    Some(DeltaBase::Ref(oid))
                }
                _ => None,
            };
            let mut body = Vec::new();
            let consumed = inflate_into(
                &pack_bytes[offset..trailer_offset],
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
            raw_entries.push((
                entry_offset as u64,
                crc32fast::hash(&pack_bytes[entry_offset..offset]),
            ));
            if let Some(base) = base {
                parsed_entries.push(ParsedPackEntry::Delta {
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
                parsed_entries.push(ParsedPackEntry::Resolved(PackObject {
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

        let resolved = resolve_pack_entries(parsed_entries, format, &mut external_base)?;
        let entries = resolved
            .iter()
            .zip(raw_entries)
            .map(|(object, (offset, crc32))| PackIndexEntry {
                oid: object.entry.oid,
                crc32,
                offset,
            })
            .collect::<Vec<_>>();
        let index = PackIndex::write_v2(format, &entries, &pack_checksum)?;
        Ok(PackIndexBuild {
            index,
            pack_checksum,
            entries,
        })
    }

    /// Validate and index a pack from the reader's current position to EOF.
    ///
    /// This produces the same v2 `.idx` bytes and object metadata as
    /// [`PackIndex::write_v2_for_pack`] without requiring the caller to provide
    /// the pack as one contiguous byte slice. The reader is left positioned at
    /// EOF on success.
    pub fn write_v2_for_pack_reader<R>(
        reader: &mut R,
        format: ObjectFormat,
    ) -> Result<PackStreamIndexBuild>
    where
        R: Read + Seek,
    {
        index_pack_from_reader(reader, format)
    }

    /// Validate and index a pack from the reader's current position, stopping
    /// after the pack trailer checksum.
    ///
    /// This is for transports where the pack length is not known in advance but
    /// the stream is expected to contain exactly one pack. It avoids forcing the
    /// caller to first materialize the pack only to learn its length.
    pub fn write_v2_for_pack_reader_to_trailer<R>(
        reader: &mut R,
        format: ObjectFormat,
    ) -> Result<PackStreamIndexBuild>
    where
        R: Read,
    {
        index_pack_from_reader_to_trailer(reader, format)
    }

    /// `write_v2_for_pack_reader_to_trailer` that reports streaming pack
    /// progress. `progress` is invoked on a throttled cadence with the pack byte
    /// offset, objects parsed so far, and the header-declared total, then once
    /// more at completion with `received_objects == total_objects`.
    pub fn write_v2_for_pack_reader_to_trailer_with_progress<R, F>(
        reader: &mut R,
        format: ObjectFormat,
        progress: F,
    ) -> Result<PackStreamIndexBuild>
    where
        R: Read,
        F: FnMut(PackStreamProgress),
    {
        index_pack_from_reader_to_trailer_with_progress(reader, format, progress)
    }

    /// Like [`Self::write_v2_for_pack_reader_to_trailer_with_progress`], but
    /// polls `cancel` cooperatively between pack objects (and after progress
    /// emission). Returns [`GitError::Cancelled`] when the flag trips.
    ///
    /// Progress callbacks remain side-effect only; cancellation is checked
    /// independently and does not require the callback to return
    /// [`sley_core::StreamControl`].
    pub fn write_v2_for_pack_reader_to_trailer_with_progress_and_cancel<R, F>(
        reader: &mut R,
        format: ObjectFormat,
        cancel: CancelFlag<'_>,
        progress: F,
    ) -> Result<PackStreamIndexBuild>
    where
        R: Read,
        F: FnMut(PackStreamProgress),
    {
        index_pack_from_reader_to_trailer_with_progress_and_cancel(reader, format, cancel, progress)
    }

    pub fn write_v2_for_pack_reader_with_len<R>(
        reader: &mut R,
        format: ObjectFormat,
        pack_len: u64,
    ) -> Result<PackStreamIndexBuild>
    where
        R: Read,
    {
        index_pack_from_reader_with_len(reader, format, pack_len)
    }

    /// Validate and index a pack from a filesystem path without loading the
    /// entire pack file into memory.
    pub fn write_v2_for_pack_path(
        path: impl AsRef<Path>,
        format: ObjectFormat,
    ) -> Result<PackStreamIndexBuild> {
        let mut file = File::open(path)?;
        Self::write_v2_for_pack_reader(&mut file, format)
    }

    pub fn parse_v2_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, true)
    }

    pub fn parse_without_checksum(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, false)
    }

    pub(crate) fn parse_impl(
        bytes: &[u8],
        format: ObjectFormat,
        verify_checksum: bool,
    ) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        if bytes[..4] != [0xff, b't', b'O', b'c'] {
            return Self::parse_v1_impl(bytes, format, verify_checksum);
        }
        if bytes.len() < 8 + 256 * 4 + 2 * hash_len {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let index_checksum_offset = bytes.len() - hash_len;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if verify_checksum {
            let actual_index_checksum =
                sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
            if actual_index_checksum != index_checksum {
                return Err(GitError::InvalidFormat(format!(
                    "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
                )));
            }
        }

        let mut offset = 8usize;
        let mut fanout = [0u32; 256];
        let mut previous = 0u32;
        for slot in &mut fanout {
            *slot = u32_be(&bytes[offset..offset + 4]);
            if *slot < previous {
                return Err(GitError::InvalidFormat(
                    "pack index fanout is not monotonic".into(),
                ));
            }
            previous = *slot;
            offset += 4;
        }
        let count = fanout[255] as usize;
        let oid_table = checked_range(offset, count, hash_len, bytes.len())?;
        offset = oid_table.end;
        let crc_table = checked_range(offset, count, 4, bytes.len())?;
        offset = crc_table.end;
        let small_offset_table = checked_range(offset, count, 4, bytes.len())?;
        offset = small_offset_table.end;

        let large_offset_count = (0..count)
            .filter(|idx| {
                let start = small_offset_table.start + idx * 4;
                u32_be(&bytes[start..start + 4]) & 0x8000_0000 != 0
            })
            .count();
        let mut large_offset_table = checked_range(offset, large_offset_count, 8, bytes.len())?;
        offset = large_offset_table.end;

        let expected_trailer_offset = bytes.len() - hash_len * 2;
        if offset != expected_trailer_offset {
            if !verify_checksum && offset < expected_trailer_offset {
                large_offset_table = large_offset_table.start..expected_trailer_offset;
                offset = expected_trailer_offset;
            } else {
                return Err(GitError::InvalidFormat(format!(
                    "pack index has {} unexpected bytes before trailer",
                    expected_trailer_offset.saturating_sub(offset)
                )));
            }
        }
        let pack_checksum = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;

        let mut entries = Vec::with_capacity(count);
        for idx in 0..count {
            let oid_start = oid_table.start + idx * hash_len;
            let crc_start = crc_table.start + idx * 4;
            let offset_start = small_offset_table.start + idx * 4;
            let oid_bytes = &bytes[oid_start..oid_start + hash_len];
            // Object ids must be non-decreasing: Git permits duplicate objects
            // in an incoming pack, and represents every copy in the index. The
            // fanout must still match the first byte so binary search cannot
            // silently miss the duplicate run.
            if idx > 0 && oid_bytes < &bytes[oid_start - hash_len..oid_start] {
                return Err(GitError::InvalidFormat(
                    "pack index object ids are not sorted".into(),
                ));
            }
            let expected_min = if oid_bytes[0] == 0 {
                0
            } else {
                fanout[usize::from(oid_bytes[0] - 1)]
            };
            if (idx as u32) < expected_min || (idx as u32) >= fanout[usize::from(oid_bytes[0])] {
                return Err(GitError::InvalidFormat(
                    "pack index object id is outside its fanout bucket".into(),
                ));
            }
            let raw_offset = u32_be(&bytes[offset_start..offset_start + 4]);
            let offset = if raw_offset & 0x8000_0000 == 0 {
                u64::from(raw_offset)
            } else {
                let large_idx = (raw_offset & 0x7fff_ffff) as usize;
                let large_start = large_offset_table.start + large_idx * 8;
                if large_idx >= large_offset_table.len() / 8 {
                    return Err(GitError::InvalidFormat(
                        "pack index large offset points past table".into(),
                    ));
                }
                u64_be(&bytes[large_start..large_start + 8])
            };
            entries.push(PackIndexEntry {
                oid: ObjectId::from_raw(format, oid_bytes)?,
                crc32: u32_be(&bytes[crc_start..crc_start + 4]),
                offset,
            });
        }
        Ok(Self {
            version,
            fanout,
            entries,
            pack_checksum,
            index_checksum,
        })
    }

    pub(crate) fn parse_v1_impl(
        bytes: &[u8],
        format: ObjectFormat,
        verify_checksum: bool,
    ) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 256 * 4 + 2 * hash_len {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let index_checksum_offset = bytes.len() - hash_len;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if verify_checksum {
            let actual_index_checksum =
                sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
            if actual_index_checksum != index_checksum {
                return Err(GitError::InvalidFormat(format!(
                    "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
                )));
            }
        }

        let mut offset = 0usize;
        let mut fanout = [0u32; 256];
        let mut previous = 0u32;
        for slot in &mut fanout {
            *slot = u32_be(&bytes[offset..offset + 4]);
            if *slot < previous {
                return Err(GitError::InvalidFormat(
                    "pack index fanout is not monotonic".into(),
                ));
            }
            previous = *slot;
            offset += 4;
        }
        let count = fanout[255] as usize;
        let entry_len = hash_len
            .checked_add(4)
            .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
        let entry_table = checked_range(offset, count, entry_len, bytes.len())?;
        offset = entry_table.end;
        let expected_trailer_offset = bytes.len() - hash_len * 2;
        if offset != expected_trailer_offset {
            return Err(GitError::InvalidFormat(format!(
                "pack index has {} unexpected bytes before trailer",
                expected_trailer_offset.saturating_sub(offset)
            )));
        }
        let pack_checksum = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;

        let mut entries = Vec::with_capacity(count);
        let mut previous_oid: Option<ObjectId> = None;
        for idx in 0..count {
            let start = entry_table.start + idx * entry_len;
            let oid = ObjectId::from_raw(format, &bytes[start + 4..start + entry_len])?;
            if let Some(previous) = &previous_oid
                && previous.as_bytes() > oid.as_bytes()
            {
                return Err(GitError::InvalidFormat(
                    "pack index object ids are not sorted".into(),
                ));
            }
            previous_oid = Some(oid);
            entries.push(PackIndexEntry {
                oid,
                crc32: 0,
                offset: u64::from(u32_be(&bytes[start..start + 4])),
            });
        }
        Ok(Self {
            version: 1,
            fanout,
            entries,
            pack_checksum,
            index_checksum,
        })
    }

    pub fn find(&self, oid: &ObjectId) -> Option<&PackIndexEntry> {
        self.entries
            .binary_search_by(|entry| entry.oid.as_bytes().cmp(oid.as_bytes()))
            .ok()
            .map(|idx| &self.entries[idx])
    }

    pub fn write_v2_sha1(entries: &[PackIndexEntry], pack_checksum: &ObjectId) -> Result<Vec<u8>> {
        Self::write_v2(ObjectFormat::Sha1, entries, pack_checksum)
    }

    pub fn write_v2(
        format: ObjectFormat,
        entries: &[PackIndexEntry],
        pack_checksum: &ObjectId,
    ) -> Result<Vec<u8>> {
        if pack_checksum.format() != format {
            return Err(GitError::InvalidObjectId(
                "pack checksum format does not match index format".into(),
            ));
        }
        let mut entries = entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        let mut fanout = [0u32; 256];
        for entry in &entries {
            if entry.oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "pack index entry format does not match index format".into(),
                ));
            }
            let first = entry.oid.as_bytes()[0] as usize;
            fanout[first] = fanout[first]
                .checked_add(1)
                .ok_or_else(|| GitError::InvalidFormat("pack index fanout overflow".into()))?;
        }
        let mut running = 0u32;
        for slot in &mut fanout {
            running = running
                .checked_add(*slot)
                .ok_or_else(|| GitError::InvalidFormat("pack index fanout overflow".into()))?;
            *slot = running;
        }

        let mut index = Vec::new();
        index.extend_from_slice(&[0xff, b't', b'O', b'c']);
        index.extend_from_slice(&2u32.to_be_bytes());
        for count in fanout {
            index.extend_from_slice(&count.to_be_bytes());
        }
        for entry in &entries {
            index.extend_from_slice(entry.oid.as_bytes());
        }
        for entry in &entries {
            index.extend_from_slice(&entry.crc32.to_be_bytes());
        }

        let mut large_offsets = Vec::new();
        for entry in &entries {
            if entry.offset < 0x8000_0000 {
                index.extend_from_slice(&(entry.offset as u32).to_be_bytes());
            } else {
                if large_offsets.len() > 0x7fff_ffff {
                    return Err(GitError::InvalidFormat(
                        "too many large pack offsets".into(),
                    ));
                }
                let large_idx = large_offsets.len() as u32;
                index.extend_from_slice(&(0x8000_0000 | large_idx).to_be_bytes());
                large_offsets.push(entry.offset);
            }
        }
        for offset in large_offsets {
            index.extend_from_slice(&offset.to_be_bytes());
        }
        index.extend_from_slice(pack_checksum.as_bytes());
        let index_checksum = sley_core::digest_bytes(format, &index)?;
        index.extend_from_slice(index_checksum.as_bytes());
        Ok(index)
    }

    /// Serialise a version-1 pack `.idx`: a 256-entry fanout, then for each
    /// object an inline 4-byte big-endian pack offset immediately followed by
    /// its object id (sorted by oid), then the pack checksum and a trailing
    /// index checksum. v1 has no CRC table and cannot represent offsets that
    /// do not fit in 32 bits.
    pub fn write_v1(
        format: ObjectFormat,
        entries: &[PackIndexEntry],
        pack_checksum: &ObjectId,
    ) -> Result<Vec<u8>> {
        if pack_checksum.format() != format {
            return Err(GitError::InvalidObjectId(
                "pack checksum format does not match index format".into(),
            ));
        }
        let mut entries = entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        let mut fanout = [0u32; 256];
        for entry in &entries {
            if entry.oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "pack index entry format does not match index format".into(),
                ));
            }
            if entry.offset > 0xffff_ffff {
                return Err(GitError::InvalidFormat(
                    "pack offset too large for a version-1 index".into(),
                ));
            }
            let first = entry.oid.as_bytes()[0] as usize;
            fanout[first] = fanout[first]
                .checked_add(1)
                .ok_or_else(|| GitError::InvalidFormat("pack index fanout overflow".into()))?;
        }
        let mut running = 0u32;
        for slot in &mut fanout {
            running = running
                .checked_add(*slot)
                .ok_or_else(|| GitError::InvalidFormat("pack index fanout overflow".into()))?;
            *slot = running;
        }

        let mut index = Vec::new();
        for count in fanout {
            index.extend_from_slice(&count.to_be_bytes());
        }
        for entry in &entries {
            index.extend_from_slice(&(entry.offset as u32).to_be_bytes());
            index.extend_from_slice(entry.oid.as_bytes());
        }
        index.extend_from_slice(pack_checksum.as_bytes());
        let index_checksum = sley_core::digest_bytes(format, &index)?;
        index.extend_from_slice(index_checksum.as_bytes());
        Ok(index)
    }
}
/// The `.rev` table for a pack: index positions (the rank of each object in
/// the oid-sorted `.idx`) listed in pack order (ascending pack offset), as
/// upstream `write_rev_file` lays them out. Accepts `entries` in any order;
/// the result feeds [`PackReverseIndex::write`].
pub fn pack_order_index_positions(entries: &[PackIndexEntry]) -> Vec<u32> {
    let mut oid_sorted: Vec<usize> = (0..entries.len()).collect();
    oid_sorted.sort_by(|&a, &b| entries[a].oid.as_bytes().cmp(entries[b].oid.as_bytes()));
    let mut index_position = vec![0u32; entries.len()];
    for (position, &entry) in oid_sorted.iter().enumerate() {
        index_position[entry] = position as u32;
    }
    let mut by_offset: Vec<usize> = (0..entries.len()).collect();
    by_offset.sort_by_key(|&entry| entries[entry].offset);
    by_offset
        .into_iter()
        .map(|entry| index_position[entry])
        .collect()
}

impl PackReverseIndex {
    pub fn write(
        format: ObjectFormat,
        positions: &[u32],
        pack_checksum: &ObjectId,
    ) -> Result<Vec<u8>> {
        if pack_checksum.format() != format {
            return Err(GitError::InvalidObjectId(
                "pack checksum format does not match reverse index format".into(),
            ));
        }
        validate_position_permutation(positions)?;

        let mut out = Vec::new();
        out.extend_from_slice(b"RIDX");
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for position in positions {
            out.extend_from_slice(&position.to_be_bytes());
        }
        out.extend_from_slice(pack_checksum.as_bytes());
        let checksum = sley_core::digest_bytes(format, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat, object_count: usize) -> Result<Self> {
        let hash_len = format.raw_len();
        let table_len = object_count
            .checked_mul(4)
            .ok_or_else(|| GitError::InvalidFormat("reverse index table overflow".into()))?;
        let min_len = 12usize
            .checked_add(table_len)
            .and_then(|len| len.checked_add(hash_len * 2))
            .ok_or_else(|| GitError::InvalidFormat("reverse index length overflow".into()))?;
        if bytes.len() < min_len {
            return Err(GitError::InvalidFormat("reverse index too short".into()));
        }
        if bytes.len() != min_len {
            return Err(GitError::InvalidFormat(format!(
                "reverse index has {} trailing bytes",
                bytes.len() - min_len
            )));
        }
        if &bytes[..4] != b"RIDX" {
            return Err(GitError::InvalidFormat("unknown signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 1 {
            return Err(GitError::InvalidFormat(format!(
                "unsupported version {version}"
            )));
        }
        let hash_id = u32_be(&bytes[8..12]);
        if hash_id != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "unsupported hash id {hash_id}"
            )));
        }

        let index_checksum_offset = bytes.len() - hash_len;
        let pack_checksum_offset = index_checksum_offset - hash_len;
        let pack_checksum =
            ObjectId::from_raw(format, &bytes[pack_checksum_offset..index_checksum_offset])?;
        let mut positions = Vec::with_capacity(object_count);
        let mut offset = 12usize;
        for _ in 0..object_count {
            let position = u32_be(&bytes[offset..offset + 4]);
            positions.push(position);
            offset += 4;
        }
        validate_position_permutation(&positions)?;

        // Structural validation intentionally precedes the checksum: fsck
        // reports a corrupted table row as an invalid rev-index position.
        let actual_index_checksum =
            sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat("invalid checksum".into()));
        }

        Ok(Self {
            version,
            format,
            positions,
            pack_checksum,
            index_checksum,
        })
    }

    /// Resolve a pack offset to its object id using this reverse index.
    ///
    /// `positions` are listed in pack offset order; each value names the
    /// oid-sorted index position for that object. When the reverse index's pack
    /// checksum does not match `index`, returns `None`.
    pub fn oid_at_offset(&self, index: &PackIndexViewData, offset: u64) -> Option<ObjectId> {
        if self.pack_checksum != index.pack_checksum {
            return None;
        }
        let view = index.as_view();
        let positions = &self.positions;
        let mut lo = 0usize;
        let mut hi = positions.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let idx_pos = positions[mid] as usize;
            let entry_offset = view.lookup_at(idx_pos)?.offset;
            if entry_offset < offset {
                lo = mid + 1;
            } else if entry_offset > offset {
                hi = mid;
            } else {
                return index.oid_at(idx_pos).ok();
            }
        }
        None
    }
}

impl PackMtimes {
    pub fn write(
        format: ObjectFormat,
        mtimes: &[u32],
        pack_checksum: &ObjectId,
    ) -> Result<Vec<u8>> {
        if pack_checksum.format() != format {
            return Err(GitError::InvalidObjectId(
                "pack checksum format does not match mtimes format".into(),
            ));
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"MTME");
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for mtime in mtimes {
            out.extend_from_slice(&mtime.to_be_bytes());
        }
        out.extend_from_slice(pack_checksum.as_bytes());
        let checksum = sley_core::digest_bytes(format, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat, object_count: usize) -> Result<Self> {
        let hash_len = format.raw_len();
        let table_len = object_count
            .checked_mul(4)
            .ok_or_else(|| GitError::InvalidFormat("mtimes table overflow".into()))?;
        let expected_len = 12usize
            .checked_add(table_len)
            .and_then(|len| len.checked_add(hash_len * 2))
            .ok_or_else(|| GitError::InvalidFormat("mtimes length overflow".into()))?;
        if bytes.len() < expected_len {
            return Err(GitError::InvalidFormat("mtimes file too short".into()));
        }
        if bytes.len() != expected_len {
            return Err(GitError::InvalidFormat(format!(
                "mtimes file has {} trailing bytes",
                bytes.len() - expected_len
            )));
        }
        if &bytes[..4] != b"MTME" {
            return Err(GitError::InvalidFormat("missing mtimes signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 1 {
            return Err(GitError::Unsupported(format!("mtimes version {version}")));
        }
        let hash_id = u32_be(&bytes[8..12]);
        if hash_id != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "mtimes hash id {hash_id} does not match {}",
                format.name()
            )));
        }

        let index_checksum_offset = bytes.len() - hash_len;
        let actual_index_checksum =
            sley_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat(format!(
                "mtimes checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
            )));
        }

        let pack_checksum_offset = index_checksum_offset - hash_len;
        let pack_checksum =
            ObjectId::from_raw(format, &bytes[pack_checksum_offset..index_checksum_offset])?;
        let mut mtimes = Vec::with_capacity(object_count);
        let mut offset = 12usize;
        for _ in 0..object_count {
            mtimes.push(u32_be(&bytes[offset..offset + 4]));
            offset += 4;
        }

        Ok(Self {
            version,
            format,
            mtimes,
            pack_checksum,
            index_checksum,
        })
    }
}

impl PackBitmapIndex {
    pub const OPTION_FULL_DAG: u16 = 0x0001;
    pub const OPTION_HASH_CACHE: u16 = 0x0004;
    pub const OPTION_LOOKUP_TABLE: u16 = 0x0010;
    pub const OPTION_PSEUDO_MERGES: u16 = 0x0020;

    pub fn parse(bytes: &[u8], format: ObjectFormat, object_count: usize) -> Result<Self> {
        let hash_len = format.raw_len();
        let min_len = 12usize
            .checked_add(hash_len * 2)
            .ok_or_else(|| GitError::InvalidFormat("bitmap index length overflow".into()))?;
        if bytes.len() < min_len {
            return Err(GitError::InvalidFormat("bitmap index too short".into()));
        }
        if &bytes[..4] != b"BITM" {
            return Err(GitError::InvalidFormat(
                "missing bitmap index signature".into(),
            ));
        }
        let version = u16_be(&bytes[4..6]);
        if version != 1 {
            return Err(GitError::Unsupported(format!(
                "bitmap index version {version}"
            )));
        }
        let options = u16_be(&bytes[6..8]);
        let known_options = Self::OPTION_FULL_DAG
            | Self::OPTION_HASH_CACHE
            | Self::OPTION_LOOKUP_TABLE
            | Self::OPTION_PSEUDO_MERGES;
        if options & !known_options != 0 {
            return Err(GitError::Unsupported(format!(
                "bitmap index options {:#06x}",
                options & !known_options
            )));
        }
        let entry_count = u32_be(&bytes[8..12]) as usize;
        let checksum_offset = bytes.len() - hash_len;
        let index_checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        let mut extras_end = checksum_offset;
        let hash_cache_range = if options & Self::OPTION_HASH_CACHE != 0 {
            let cache_len = object_count
                .checked_mul(4)
                .ok_or_else(|| GitError::InvalidFormat("bitmap hash cache overflow".into()))?;
            if cache_len > extras_end {
                return Err(GitError::InvalidFormat(
                    "truncated bitmap hash cache".into(),
                ));
            }
            extras_end -= cache_len;
            Some(extras_end..extras_end + cache_len)
        } else {
            None
        };
        let lookup_table = options & Self::OPTION_LOOKUP_TABLE != 0;
        let lookup_table_range = if lookup_table {
            let table_len = entry_count
                .checked_mul(16)
                .ok_or_else(|| GitError::InvalidFormat("bitmap lookup table overflow".into()))?;
            if table_len > extras_end {
                return Err(GitError::InvalidFormat(
                    "truncated bitmap lookup table".into(),
                ));
            }
            extras_end -= table_len;
            Some(extras_end..extras_end + table_len)
        } else {
            None
        };
        let pseudo_merge_range = if options & Self::OPTION_PSEUDO_MERGES != 0 {
            if extras_end < 24 {
                return Err(GitError::InvalidFormat(
                    "truncated bitmap pseudo-merge extension".into(),
                ));
            }
            let extension_size = u64_be(&bytes[extras_end - 8..extras_end]) as usize;
            if extension_size > extras_end {
                return Err(GitError::InvalidFormat(
                    "bitmap pseudo-merge extension points before file start".into(),
                ));
            }
            let start = extras_end - extension_size;
            Some(start..extras_end)
        } else {
            None
        };
        let entries_end = pseudo_merge_range
            .as_ref()
            .map(|range| range.start)
            .unwrap_or(extras_end);

        let pack_checksum_end = 12usize
            .checked_add(hash_len)
            .ok_or_else(|| GitError::InvalidFormat("bitmap index length overflow".into()))?;
        let pack_checksum = ObjectId::from_raw(format, &bytes[12..pack_checksum_end])?;
        let mut offset = pack_checksum_end;
        let commits = parse_bitmap_ewah(bytes, &mut offset, entries_end, object_count)?;
        let trees = parse_bitmap_ewah(bytes, &mut offset, entries_end, object_count)?;
        let blobs = parse_bitmap_ewah(bytes, &mut offset, entries_end, object_count)?;
        let tags = parse_bitmap_ewah(bytes, &mut offset, entries_end, object_count)?;

        let mut entries = Vec::with_capacity(entry_count);
        for idx in 0..entry_count {
            if entries_end.saturating_sub(offset) < 6 {
                return Err(GitError::InvalidFormat(
                    "truncated bitmap index entry".into(),
                ));
            }
            let object_position = u32_be(&bytes[offset..offset + 4]);
            offset += 4;
            if object_position as usize >= object_count {
                return Err(GitError::InvalidFormat(
                    "bitmap index entry points past object table".into(),
                ));
            }
            let xor_offset = bytes[offset];
            offset += 1;
            if xor_offset as usize > idx || xor_offset > 160 {
                return Err(GitError::InvalidFormat(
                    "bitmap index entry has invalid XOR offset".into(),
                ));
            }
            let flags = bytes[offset];
            offset += 1;
            let bitmap = parse_bitmap_ewah(bytes, &mut offset, entries_end, object_count)?;
            entries.push(PackBitmapEntry {
                object_position,
                xor_offset,
                flags,
                bitmap,
            });
        }

        if offset != entries_end {
            return Err(GitError::InvalidFormat(format!(
                "bitmap index has {} trailing entry bytes",
                entries_end - offset
            )));
        }

        let pseudo_merges = if let Some(range) = pseudo_merge_range {
            parse_bitmap_pseudo_merges(bytes, range, object_count)?
        } else {
            Vec::new()
        };

        let name_hash_cache = if let Some(range) = hash_cache_range {
            let mut cache = Vec::with_capacity(object_count);
            let mut offset = range.start;
            for _ in 0..object_count {
                cache.push(u32_be(&bytes[offset..offset + 4]));
                offset += 4;
            }
            Some(cache)
        } else {
            None
        };
        if let Some(range) = lookup_table_range {
            for row in bytes[range].chunks_exact(16) {
                let commit_position = u32_be(&row[..4]);
                let entry_offset = u64_be(&row[4..12]);
                let xor_row = u32_be(&row[12..16]);
                if commit_position as usize >= object_count
                    || entry_offset as usize >= entries_end
                    || (xor_row != u32::MAX && xor_row as usize >= entry_count)
                {
                    return Err(GitError::InvalidFormat(
                        "corrupt bitmap lookup table".into(),
                    ));
                }
            }
        }

        let actual_index_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat(format!(
                "bitmap index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
            )));
        }

        Ok(Self {
            version,
            format,
            options,
            pack_checksum,
            index_checksum,
            type_bitmaps: PackBitmapTypeBitmaps {
                commits,
                trees,
                blobs,
                tags,
            },
            entries,
            pseudo_merges,
            lookup_table,
            name_hash_cache,
        })
    }

    /// Looks up the stored entry whose commit sits at `position` in the
    /// oid-sorted pack index (`.idx` order; see [`PackBitmapEntry::object_position`]).
    pub fn entry_for_index_position(&self, position: u32) -> Option<&PackBitmapEntry> {
        self.entries
            .iter()
            .find(|entry| entry.object_position == position)
    }
}

pub(crate) fn parse_bitmap_pseudo_merges(
    bytes: &[u8],
    range: std::ops::Range<usize>,
    object_count: usize,
) -> Result<Vec<PackBitmapPseudoMerge>> {
    if range.end < range.start || range.end > bytes.len() || range.end - range.start < 24 {
        return Err(GitError::InvalidFormat(
            "truncated bitmap pseudo-merge extension".into(),
        ));
    }
    let trailer_start = range.end - 24;
    let pseudo_merge_count = u32_be(&bytes[trailer_start..trailer_start + 4]) as usize;
    let commit_count = u32_be(&bytes[trailer_start + 4..trailer_start + 8]) as usize;
    let lookup_offset = u64_be(&bytes[trailer_start + 8..trailer_start + 16]) as usize;
    let extension_size = u64_be(&bytes[trailer_start + 16..trailer_start + 24]) as usize;
    if extension_size != range.end - range.start {
        return Err(GitError::InvalidFormat(
            "bitmap pseudo-merge extension size mismatch".into(),
        ));
    }
    let lookup_start = range
        .start
        .checked_add(lookup_offset)
        .ok_or_else(|| GitError::InvalidFormat("bitmap pseudo-merge lookup overflow".into()))?;
    if lookup_start > trailer_start {
        return Err(GitError::InvalidFormat(
            "bitmap pseudo-merge lookup points past extension".into(),
        ));
    }
    let lookup_len = commit_count
        .checked_mul(12)
        .ok_or_else(|| GitError::InvalidFormat("bitmap pseudo-merge lookup overflow".into()))?;
    if lookup_start
        .checked_add(lookup_len)
        .is_none_or(|end| end > trailer_start)
    {
        return Err(GitError::InvalidFormat(
            "truncated bitmap pseudo-merge lookup".into(),
        ));
    }
    let position_table_len = pseudo_merge_count.checked_mul(8).ok_or_else(|| {
        GitError::InvalidFormat("bitmap pseudo-merge position table overflow".into())
    })?;
    let position_table_start = trailer_start
        .checked_sub(position_table_len)
        .filter(|start| *start >= range.start)
        .ok_or_else(|| {
            GitError::InvalidFormat("truncated bitmap pseudo-merge position table".into())
        })?;

    let mut pseudo_merges = Vec::with_capacity(pseudo_merge_count);
    let mut cursor = position_table_start;
    for _ in 0..pseudo_merge_count {
        let pseudo_offset = u64_be(&bytes[cursor..cursor + 8]) as usize;
        cursor += 8;
        if pseudo_offset < range.start || pseudo_offset >= position_table_start {
            return Err(GitError::InvalidFormat(
                "bitmap pseudo-merge offset out of range".into(),
            ));
        }
        let mut offset = pseudo_offset;
        let commits = parse_bitmap_ewah(bytes, &mut offset, range.end, object_count)?;
        let bitmap = parse_bitmap_ewah(bytes, &mut offset, range.end, object_count)?;
        pseudo_merges.push(PackBitmapPseudoMerge { commits, bitmap });
    }
    Ok(pseudo_merges)
}

pub(crate) fn parse_bitmap_ewah(
    bytes: &[u8],
    offset: &mut usize,
    checksum_offset: usize,
    _object_count: usize,
) -> Result<EwahBitmap> {
    if checksum_offset.saturating_sub(*offset) < 12 {
        return Err(GitError::InvalidFormat("truncated EWAH bitmap".into()));
    }
    let bit_size = u32_be(&bytes[*offset..*offset + 4]);
    *offset += 4;
    let word_count = u32_be(&bytes[*offset..*offset + 4]) as usize;
    *offset += 4;
    let words_len = word_count
        .checked_mul(8)
        .ok_or_else(|| GitError::InvalidFormat("EWAH word table overflow".into()))?;
    if checksum_offset.saturating_sub(*offset) < words_len + 4 {
        return Err(GitError::InvalidFormat("truncated EWAH word table".into()));
    }
    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        words.push(u64_be(&bytes[*offset..*offset + 8]));
        *offset += 8;
    }
    let rlw_position = u32_be(&bytes[*offset..*offset + 4]);
    *offset += 4;
    validate_ewah_words(bit_size, &words, rlw_position)?;
    Ok(EwahBitmap {
        bit_size,
        words,
        rlw_position,
    })
}

pub(crate) fn validate_ewah_words(bit_size: u32, words: &[u64], rlw_position: u32) -> Result<()> {
    if words.is_empty() {
        if rlw_position != 0 || bit_size != 0 {
            return Err(GitError::InvalidFormat(
                "EWAH bitmap has invalid empty RLW".into(),
            ));
        }
        return Ok(());
    }
    if rlw_position as usize >= words.len() {
        return Err(GitError::InvalidFormat(
            "EWAH RLW position points past word table".into(),
        ));
    }
    let mut word_idx = 0usize;
    let mut decoded_words = 0u64;
    while word_idx < words.len() {
        let rlw = words[word_idx];
        let run_words = (rlw >> 1) & 0xffff_ffff;
        let literal_words = (rlw >> 33) as usize;
        word_idx += 1;
        word_idx = word_idx
            .checked_add(literal_words)
            .ok_or_else(|| GitError::InvalidFormat("EWAH literal word overflow".into()))?;
        if word_idx > words.len() {
            return Err(GitError::InvalidFormat(
                "EWAH literal words extend past word table".into(),
            ));
        }
        decoded_words = decoded_words
            .checked_add(run_words)
            .and_then(|value| value.checked_add(literal_words as u64))
            .ok_or_else(|| GitError::InvalidFormat("EWAH decoded size overflow".into()))?;
    }
    let decoded_bits = decoded_words
        .checked_mul(64)
        .ok_or_else(|| GitError::InvalidFormat("EWAH decoded bit size overflow".into()))?;
    if decoded_bits < u64::from(bit_size) {
        return Err(GitError::InvalidFormat(
            "EWAH bitmap decodes fewer bits than declared".into(),
        ));
    }
    Ok(())
}

impl MultiPackIndex {
    pub fn write(
        format: ObjectFormat,
        version: u8,
        pack_names: &[String],
        objects: &[MultiPackIndexEntry],
    ) -> Result<Vec<u8>> {
        Self::write_with_reverse_index(format, version, pack_names, objects, None)
    }

    /// Like [`MultiPackIndex::write`], but when `preferred_pack` is `Some`,
    /// additionally emits the `RIDX` chunk: the object order a multi-pack
    /// `.bitmap` numbers its bits in ("pseudo-pack order" — every object of
    /// the preferred pack first, then the rest by pack id, each pack's slice
    /// in offset order), stored as one u32 midx position per object.
    ///
    /// `preferred_pack` is the pack-int-id receiving pseudo-pack priority; it
    /// must be in range.
    pub fn write_with_reverse_index(
        format: ObjectFormat,
        version: u8,
        pack_names: &[String],
        objects: &[MultiPackIndexEntry],
        preferred_pack: Option<u32>,
    ) -> Result<Vec<u8>> {
        Self::write_with_bitmap_packs(format, version, pack_names, objects, preferred_pack, None)
    }

    pub fn write_with_bitmap_packs(
        format: ObjectFormat,
        version: u8,
        pack_names: &[String],
        objects: &[MultiPackIndexEntry],
        preferred_pack: Option<u32>,
        bitmapped_packs: Option<&[MultiPackBitmapPack]>,
    ) -> Result<Vec<u8>> {
        if let Some(preferred) = preferred_pack
            && preferred as usize >= pack_names.len()
        {
            return Err(GitError::InvalidFormat(format!(
                "preferred pack {preferred} out of range for {} packs",
                pack_names.len()
            )));
        }
        if version != 1 && version != 2 {
            return Err(GitError::Unsupported(format!(
                "multi-pack-index version {version}"
            )));
        }
        if pack_names.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat(
                "too many multi-pack-index packs".into(),
            ));
        }
        if objects.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat(
                "too many multi-pack-index objects".into(),
            ));
        }
        if let Some(bitmapped_packs) = bitmapped_packs {
            if bitmapped_packs.len() != pack_names.len() {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index BTMP pack count mismatch".into(),
                ));
            }
            for pack in bitmapped_packs {
                let bitmap_end = u64::from(pack.bitmap_pos)
                    .checked_add(u64::from(pack.bitmap_nr))
                    .ok_or_else(|| {
                        GitError::InvalidFormat("multi-pack-index BTMP range overflow".into())
                    })?;
                if bitmap_end > objects.len() as u64 {
                    return Err(GitError::InvalidFormat(
                        "multi-pack-index BTMP range points past object table".into(),
                    ));
                }
            }
        }
        validate_midx_pack_names(pack_names)?;
        if version == 1 && pack_names.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(GitError::InvalidFormat(
                "multi-pack-index v1 pack names must be sorted".into(),
            ));
        }

        let mut objects = objects.iter().collect::<Vec<_>>();
        objects.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        let mut previous_oid: Option<&ObjectId> = None;
        for object in &objects {
            if object.oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "multi-pack-index object format does not match index format".into(),
                ));
            }
            if let Some(previous) = previous_oid
                && previous.as_bytes() == object.oid.as_bytes()
            {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index contains duplicate object ids".into(),
                ));
            }
            if object.pack_int_id as usize >= pack_names.len() {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index object points past pack table".into(),
                ));
            }
            previous_oid = Some(&object.oid);
        }

        let mut large_offsets = Vec::new();
        let mut chunks = vec![
            (*b"PNAM", write_midx_pack_names(pack_names)),
            (*b"OIDF", write_midx_oid_fanout(&objects)?),
            (*b"OIDL", write_midx_oid_lookup(&objects)),
            (
                *b"OOFF",
                write_midx_object_offsets(&objects, &mut large_offsets)?,
            ),
        ];
        if !large_offsets.is_empty() {
            chunks.push((*b"LOFF", large_offsets));
        }
        if let Some(preferred) = preferred_pack {
            // `objects` is already in midx (oid-sorted) order here; the chunk
            // lists each object's midx position in pseudo-pack order.
            let mut pseudo: Vec<u32> = (0..objects.len() as u32).collect();
            pseudo.sort_by_key(|&midx_pos| {
                let object = objects[midx_pos as usize];
                (
                    object.pack_int_id != preferred,
                    object.pack_int_id,
                    object.offset,
                )
            });
            let mut ridx = Vec::with_capacity(pseudo.len() * 4);
            for midx_pos in pseudo {
                ridx.extend_from_slice(&midx_pos.to_be_bytes());
            }
            chunks.push((*b"RIDX", ridx));
        }
        if let Some(bitmapped_packs) = bitmapped_packs {
            let mut btmp = Vec::with_capacity(bitmapped_packs.len() * 8);
            for pack in bitmapped_packs {
                btmp.extend_from_slice(&pack.bitmap_pos.to_be_bytes());
                btmp.extend_from_slice(&pack.bitmap_nr.to_be_bytes());
            }
            chunks.push((*b"BTMP", btmp));
        }
        write_multi_pack_index_chunks(format, version, pack_names.len() as u32, &chunks)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, true)
    }

    pub fn parse_without_checksum(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_impl(bytes, format, false)
    }

    pub(crate) fn parse_impl(
        bytes: &[u8],
        format: ObjectFormat,
        verify_checksum: bool,
    ) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 12 + 12 + hash_len {
            return Err(GitError::InvalidFormat(
                "multi-pack-index file too short".into(),
            ));
        }
        if &bytes[..4] != b"MIDX" {
            return Err(GitError::InvalidFormat(
                "missing multi-pack-index signature".into(),
            ));
        }
        let version = bytes[4];
        if version != 1 && version != 2 {
            return Err(GitError::Unsupported(format!(
                "multi-pack-index version {version}"
            )));
        }
        let hash_id = bytes[5];
        if u32::from(hash_id) != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "multi-pack-index hash id {hash_id} does not match {}",
                format.name()
            )));
        }
        let chunk_count = bytes[6] as usize;
        let base_midx_count = bytes[7];
        if base_midx_count != 0 {
            return Err(GitError::Unsupported(format!(
                "multi-pack-index base count {base_midx_count}"
            )));
        }
        let pack_count = u32_be(&bytes[8..12]);
        let lookup_len = (chunk_count + 1)
            .checked_mul(12)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?;
        let data_start = 12usize
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?;
        let checksum_offset = bytes.len() - hash_len;
        if data_start > checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated multi-pack-index chunk lookup".into(),
            ));
        }

        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if verify_checksum {
            let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
            if actual_checksum != checksum {
                return Err(GitError::InvalidFormat(format!(
                    "multi-pack-index checksum mismatch: expected {checksum}, got {actual_checksum}"
                )));
            }
        }

        let mut entries = Vec::with_capacity(chunk_count + 1);
        let mut offset = 12usize;
        for _ in 0..=chunk_count {
            let id = [
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ];
            let chunk_offset = u64_be(&bytes[offset + 4..offset + 12]);
            entries.push((id, chunk_offset));
            offset += 12;
        }
        let Some((terminator_id, terminator_offset)) = entries.last().copied() else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index chunk lookup is empty".into(),
            ));
        };
        if terminator_id != [0, 0, 0, 0] {
            return Err(GitError::InvalidFormat(
                "multi-pack-index chunk lookup missing terminator".into(),
            ));
        }
        if terminator_offset != checksum_offset as u64 {
            return Err(GitError::InvalidFormat(
                "multi-pack-index terminator does not point at checksum".into(),
            ));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut previous_offset = data_start as u64;
        let mut reported_unaligned = false;
        for pair in entries.windows(2) {
            let (id, chunk_offset) = pair[0];
            let (_next_id, next_offset) = pair[1];
            if id == [0, 0, 0, 0] {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk id is zero before terminator".into(),
                ));
            }
            if chunk_offset < data_start as u64 || chunk_offset < previous_offset {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk offsets are not monotonic".into(),
                ));
            }
            if chunk_offset % 4 != 0 && !reported_unaligned {
                eprintln!(
                    "error: chunk id {:08x} not 4-byte aligned",
                    u32::from_be_bytes(id)
                );
                reported_unaligned = true;
            }
            if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk length is invalid".into(),
                ));
            }
            chunks.push(MultiPackIndexChunk {
                id,
                offset: chunk_offset,
                len: next_offset - chunk_offset,
            });
            previous_offset = chunk_offset;
        }

        let pack_names = parse_midx_pack_names(bytes, &chunks, pack_count as usize, version)?;
        let (fanout, object_count) = parse_midx_oid_fanout(bytes, &chunks)?;
        let object_ids = parse_midx_object_ids(bytes, &chunks, format, object_count, &fanout)?;
        let objects = parse_midx_object_offsets(bytes, &chunks, object_ids, pack_count)?;
        let reverse_index = parse_midx_reverse_index(bytes, &chunks, object_count)?;
        let bitmapped_packs =
            parse_midx_bitmapped_packs(bytes, &chunks, pack_count as usize, object_count)?;

        Ok(Self {
            version,
            format,
            pack_count,
            pack_names,
            object_count: object_count as u32,
            fanout,
            objects,
            reverse_index,
            bitmapped_packs,
            chunks,
            checksum,
        })
    }

    pub fn find(&self, oid: &ObjectId) -> Option<&MultiPackIndexEntry> {
        self.objects
            .binary_search_by(|entry| entry.oid.as_bytes().cmp(oid.as_bytes()))
            .ok()
            .map(|idx| &self.objects[idx])
    }
}

impl MultiPackIndexOidLookup {
    pub fn parse(bytes: Arc<dyn PackIndexByteSource>, format: ObjectFormat) -> Result<Self> {
        let raw = bytes.as_bytes();
        let hash_len = format.raw_len();
        if raw.len() < 12 + 12 + hash_len {
            return Err(GitError::InvalidFormat(
                "multi-pack-index file too short".into(),
            ));
        }
        if &raw[..4] != b"MIDX" {
            return Err(GitError::InvalidFormat(
                "missing multi-pack-index signature".into(),
            ));
        }
        let version = raw[4];
        if version != 1 && version != 2 {
            return Err(GitError::Unsupported(format!(
                "multi-pack-index version {version}"
            )));
        }
        let hash_id = raw[5];
        if u32::from(hash_id) != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "multi-pack-index hash id {hash_id} does not match {}",
                format.name()
            )));
        }
        let chunk_count = raw[6] as usize;
        let base_midx_count = raw[7];
        if base_midx_count != 0 {
            return Err(GitError::Unsupported(format!(
                "multi-pack-index base count {base_midx_count}"
            )));
        }
        let pack_count = u32_be(&raw[8..12]);
        let lookup_len = (chunk_count + 1)
            .checked_mul(12)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?;
        let data_start = 12usize
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?;
        let checksum_offset = raw.len() - hash_len;
        if data_start > checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated multi-pack-index chunk lookup".into(),
            ));
        }

        let mut entries = Vec::with_capacity(chunk_count + 1);
        let mut offset = 12usize;
        for _ in 0..=chunk_count {
            let id = [
                raw[offset],
                raw[offset + 1],
                raw[offset + 2],
                raw[offset + 3],
            ];
            let chunk_offset = u64_be(&raw[offset + 4..offset + 12]);
            entries.push((id, chunk_offset));
            offset += 12;
        }
        let Some((terminator_id, terminator_offset)) = entries.last().copied() else {
            return Err(GitError::InvalidFormat(
                "multi-pack-index chunk lookup is empty".into(),
            ));
        };
        if terminator_id != [0, 0, 0, 0] {
            return Err(GitError::InvalidFormat(
                "multi-pack-index chunk lookup missing terminator".into(),
            ));
        }
        if terminator_offset != checksum_offset as u64 {
            return Err(GitError::InvalidFormat(
                "multi-pack-index terminator does not point at checksum".into(),
            ));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut previous_offset = data_start as u64;
        let mut reported_unaligned = false;
        for pair in entries.windows(2) {
            let (id, chunk_offset) = pair[0];
            let (_next_id, next_offset) = pair[1];
            if id == [0, 0, 0, 0] {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk id is zero before terminator".into(),
                ));
            }
            if chunk_offset < data_start as u64 || chunk_offset < previous_offset {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk offsets are not monotonic".into(),
                ));
            }
            if chunk_offset % 4 != 0 && !reported_unaligned {
                eprintln!(
                    "error: chunk id {:08x} not 4-byte aligned",
                    u32::from_be_bytes(id)
                );
                reported_unaligned = true;
            }
            if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index chunk length is invalid".into(),
                ));
            }
            chunks.push(MultiPackIndexChunk {
                id,
                offset: chunk_offset,
                len: next_offset - chunk_offset,
            });
            previous_offset = chunk_offset;
        }

        let pack_names = parse_midx_pack_names(raw, &chunks, pack_count as usize, version)?;
        let (fanout, object_count) = parse_midx_oid_fanout(raw, &chunks)?;
        let oid_lookup = midx_chunk_data(raw, &chunks, *b"OIDL", true)?
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OIDL chunk".into()))?;
        let expected_len = object_count.checked_mul(hash_len).ok_or_else(|| {
            GitError::InvalidFormat("multi-pack-index OIDL chunk overflow".into())
        })?;
        if oid_lookup.len() != expected_len {
            return Err(GitError::InvalidFormat(
                "error: multi-pack-index OID lookup chunk is the wrong size\nfatal: multi-pack-index required OID lookup chunk missing or corrupted".into(),
            ));
        }
        let object_offsets = midx_chunk_data(raw, &chunks, *b"OOFF", true)?
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OOFF chunk".into()))?;
        let expected_offsets_len = object_count.checked_mul(8).ok_or_else(|| {
            GitError::InvalidFormat("multi-pack-index OOFF chunk overflow".into())
        })?;
        if object_offsets.len() != expected_offsets_len {
            return Err(GitError::InvalidFormat(
                "error: multi-pack-index object offset chunk is the wrong size\nfatal: multi-pack-index required object offsets chunk missing or corrupted".into(),
            ));
        }
        let large_offsets = midx_chunk_data(raw, &chunks, *b"LOFF", false)?;
        if let Some(large_offsets) = large_offsets
            && large_offsets.len() % 8 != 0
        {
            return Err(GitError::InvalidFormat(
                "multi-pack-index LOFF chunk has invalid length".into(),
            ));
        }
        let oid_lookup_offset = oid_lookup.as_ptr() as usize - raw.as_ptr() as usize;
        let object_offsets_offset = object_offsets.as_ptr() as usize - raw.as_ptr() as usize;
        let (large_offsets_offset, large_offsets_len) = match large_offsets {
            Some(large_offsets) => (
                Some(large_offsets.as_ptr() as usize - raw.as_ptr() as usize),
                large_offsets.len(),
            ),
            None => (None, 0),
        };
        Ok(Self {
            format,
            pack_count,
            pack_names,
            fanout,
            object_count,
            oid_lookup_offset,
            object_offsets_offset,
            large_offsets_offset,
            large_offsets_len,
            bytes,
        })
    }

    pub fn contains(&self, oid: &ObjectId) -> bool {
        self.find_position(oid).is_some()
    }

    pub fn find(&self, oid: &ObjectId) -> Result<Option<MultiPackIndexEntry>> {
        let Some(position) = self.find_position(oid) else {
            return Ok(None);
        };
        let bytes = self.bytes.as_bytes();
        let hash_len = self.format.raw_len();
        let oid_start = self
            .oid_lookup_offset
            .checked_add(position * hash_len)
            .ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index OIDL offset overflow".into())
            })?;
        let oid = ObjectId::from_raw(self.format, &bytes[oid_start..oid_start + hash_len])?;
        let offset_start = self
            .object_offsets_offset
            .checked_add(position * 8)
            .ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index OOFF offset overflow".into())
            })?;
        let data = &bytes[offset_start..offset_start + 8];
        let pack_int_id = u32_be(&data[..4]);
        if pack_int_id >= self.pack_count {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        }
        let raw_offset = u32_be(&data[4..8]);
        let offset = if raw_offset & 0x8000_0000 == 0 {
            u64::from(raw_offset)
        } else {
            let Some(large_offsets_offset) = self.large_offsets_offset else {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index large offset missing LOFF chunk".into(),
                ));
            };
            let large_idx = (raw_offset & 0x7fff_ffff) as usize;
            let large_start = large_idx.checked_mul(8).ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index LOFF index overflow".into())
            })?;
            let large_end = large_start.checked_add(8).ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index LOFF index overflow".into())
            })?;
            if large_end > self.large_offsets_len {
                return Err(GitError::InvalidFormat(
                    "fatal: multi-pack-index large offset out of bounds".into(),
                ));
            }
            let start = large_offsets_offset + large_start;
            u64_be(&bytes[start..start + 8])
        };
        Ok(Some(MultiPackIndexEntry {
            oid,
            pack_int_id,
            offset,
            force_large_offset: raw_offset & 0x8000_0000 != 0,
        }))
    }

    pub fn pack_name(&self, pack_int_id: u32) -> Option<&str> {
        self.pack_names
            .get(pack_int_id as usize)
            .map(String::as_str)
    }

    pub(crate) fn find_position(&self, oid: &ObjectId) -> Option<usize> {
        if oid.format() != self.format || self.object_count == 0 {
            return None;
        }
        let first = oid.as_bytes()[0] as usize;
        let start = if first == 0 {
            0
        } else {
            self.fanout[first - 1] as usize
        };
        let end = self.fanout[first] as usize;
        if start >= end || end > self.object_count {
            return None;
        }
        let hash_len = self.format.raw_len();
        let table_start = self.oid_lookup_offset;
        let table_end = table_start + self.object_count * hash_len;
        let bytes = self.bytes.as_bytes();
        let table = &bytes[table_start..table_end];
        let needle = oid.as_bytes();
        let mut low = start;
        let mut high = end;
        while low < high {
            let mid = low + (high - low) / 2;
            let raw = &table[mid * hash_len..(mid + 1) * hash_len];
            match raw.cmp(needle) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        None
    }
}

pub(crate) fn validate_midx_pack_names(pack_names: &[String]) -> Result<()> {
    for name in pack_names {
        if name.is_empty() {
            return Err(GitError::InvalidFormat(
                "multi-pack-index pack name is empty".into(),
            ));
        }
        if name
            .bytes()
            .any(|byte| byte == 0 || matches!(byte, b'/' | b'\\'))
        {
            return Err(GitError::InvalidFormat(
                "multi-pack-index pack name contains an invalid byte".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn write_midx_pack_names(pack_names: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for name in pack_names {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

pub(crate) fn write_midx_oid_fanout(objects: &[&MultiPackIndexEntry]) -> Result<Vec<u8>> {
    let mut counts = [0u32; 256];
    for object in objects {
        let first = object.oid.as_bytes()[0] as usize;
        counts[first] = counts[first]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index fanout overflow".into()))?;
    }
    let mut running = 0u32;
    let mut out = Vec::with_capacity(256 * 4);
    for count in counts {
        running = running
            .checked_add(count)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index fanout overflow".into()))?;
        out.extend_from_slice(&running.to_be_bytes());
    }
    Ok(out)
}

pub(crate) fn write_midx_oid_lookup(objects: &[&MultiPackIndexEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for object in objects {
        out.extend_from_slice(object.oid.as_bytes());
    }
    out
}

pub(crate) fn write_midx_object_offsets(
    objects: &[&MultiPackIndexEntry],
    large_offsets: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for object in objects {
        out.extend_from_slice(&object.pack_int_id.to_be_bytes());
        if object.offset < 0x8000_0000 && !object.force_large_offset {
            out.extend_from_slice(&(object.offset as u32).to_be_bytes());
        } else {
            let large_idx = large_offsets.len() / 8;
            if large_idx > 0x7fff_ffff {
                return Err(GitError::InvalidFormat(
                    "too many multi-pack-index large offsets".into(),
                ));
            }
            out.extend_from_slice(&(0x8000_0000 | large_idx as u32).to_be_bytes());
            large_offsets.extend_from_slice(&object.offset.to_be_bytes());
        }
    }
    Ok(out)
}

pub(crate) fn write_multi_pack_index_chunks(
    format: ObjectFormat,
    version: u8,
    pack_count: u32,
    chunks: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>> {
    if chunks.len() > u8::MAX as usize {
        return Err(GitError::InvalidFormat(
            "too many multi-pack-index chunks".into(),
        ));
    }
    let lookup_len = (chunks.len() + 1)
        .checked_mul(12)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?;
    let mut out = Vec::new();
    out.extend_from_slice(b"MIDX");
    out.push(version);
    out.push(hash_function_id(format) as u8);
    out.push(chunks.len() as u8);
    out.push(0);
    out.extend_from_slice(&pack_count.to_be_bytes());
    let mut chunk_offset = (12usize)
        .checked_add(lookup_len)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index lookup overflow".into()))?
        as u64;
    for (id, data) in chunks {
        out.extend_from_slice(id);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        chunk_offset = chunk_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index size overflow".into()))?;
    }
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&chunk_offset.to_be_bytes());
    for (_id, data) in chunks {
        out.extend_from_slice(data);
    }
    let checksum = sley_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok(out)
}
pub(crate) fn read_pack_index_fanout(bytes: &[u8], offset: &mut usize) -> Result<[u32; 256]> {
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for slot in &mut fanout {
        *slot = u32_be(&bytes[*offset..*offset + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "pack index fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
        *offset += 4;
    }
    Ok(fanout)
}

pub(crate) fn validate_pack_index_oid_fanout(
    idx: usize,
    oid_bytes: &[u8],
    fanout: &[u32; 256],
) -> Result<()> {
    let expected_min = if oid_bytes[0] == 0 {
        0
    } else {
        fanout[usize::from(oid_bytes[0] - 1)]
    };
    if (idx as u32) < expected_min || (idx as u32) >= fanout[usize::from(oid_bytes[0])] {
        return Err(GitError::InvalidFormat(
            "pack index object id is outside its fanout bucket".into(),
        ));
    }
    Ok(())
}

pub(crate) fn pack_index_v2_offset(raw_offset: u32, large_offset_table: &[u8]) -> Result<u64> {
    if raw_offset & 0x8000_0000 == 0 {
        return Ok(u64::from(raw_offset));
    }
    let large_idx = (raw_offset & 0x7fff_ffff) as usize;
    let large_start = large_idx
        .checked_mul(8)
        .ok_or_else(|| GitError::InvalidFormat("pack index large offset overflow".into()))?;
    let large_end = large_start
        .checked_add(8)
        .ok_or_else(|| GitError::InvalidFormat("pack index large offset overflow".into()))?;
    if large_end > large_offset_table.len() {
        return Err(GitError::InvalidFormat(
            "pack index large offset points past table".into(),
        ));
    }
    Ok(u64_be(&large_offset_table[large_start..large_end]))
}
pub(crate) fn parse_midx_pack_names(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
    pack_count: usize,
    version: u8,
) -> Result<Vec<String>> {
    let data = midx_chunk_data(bytes, chunks, *b"PNAM", true)?
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing PNAM chunk".into()))?;
    let mut names = Vec::with_capacity(pack_count);
    let mut offset = 0usize;
    while names.len() < pack_count {
        let Some(relative_end) = data[offset..].iter().position(|byte| *byte == 0) else {
            return Err(GitError::InvalidFormat(
                "fatal: multi-pack-index pack-name chunk is too short".into(),
            ));
        };
        let name_bytes = &data[offset..offset + relative_end];
        if name_bytes.is_empty() {
            return Err(GitError::InvalidFormat(
                "multi-pack-index PNAM entry is empty".into(),
            ));
        }
        let name = std::str::from_utf8(name_bytes)
            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        if name.bytes().any(|byte| matches!(byte, b'/' | b'\\')) {
            return Err(GitError::InvalidFormat(
                "multi-pack-index PNAM entry contains a path separator".into(),
            ));
        }
        names.push(name.to_string());
        offset += relative_end + 1;
    }
    let padding = &data[offset..];
    if padding.len() > 3 || padding.iter().any(|byte| *byte != 0) {
        return Err(GitError::InvalidFormat(
            "multi-pack-index PNAM padding is invalid".into(),
        ));
    }
    if version == 1 && names.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(GitError::InvalidFormat(
            "multi-pack-index v1 PNAM entries are not sorted".into(),
        ));
    }
    Ok(names)
}

pub(crate) fn parse_midx_oid_fanout(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
) -> Result<([u32; 256], usize)> {
    let data = midx_chunk_data(bytes, chunks, *b"OIDF", true)?
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OIDF chunk".into()))?;
    if data.len() != 256 * 4 {
        return Err(GitError::InvalidFormat(
            "error: multi-pack-index OID fanout is of the wrong size\nfatal: multi-pack-index required OID fanout chunk missing or corrupted".into(),
        ));
    }
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = u32_be(&data[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(format!(
                "error: oid fanout out of order: fanout[{}] = {:x} > {:x} = fanout[{idx}]\nfatal: multi-pack-index required OID fanout chunk missing or corrupted",
                idx - 1,
                previous,
                *slot
            )));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255] as usize))
}

pub(crate) fn parse_midx_object_ids(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
    format: ObjectFormat,
    object_count: usize,
    fanout: &[u32; 256],
) -> Result<Vec<ObjectId>> {
    let data = midx_chunk_data(bytes, chunks, *b"OIDL", true)?
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OIDL chunk".into()))?;
    let expected_len = object_count
        .checked_mul(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index OIDL chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "error: multi-pack-index OID lookup chunk is the wrong size\nfatal: multi-pack-index required OID lookup chunk missing or corrupted".into(),
        ));
    }

    let mut ids = Vec::with_capacity(object_count);
    let mut counts = [0u32; 256];
    let mut previous_oid: Option<ObjectId> = None;
    for idx in 0..object_count {
        let start = idx * format.raw_len();
        let oid = ObjectId::from_raw(format, &data[start..start + format.raw_len()])?;
        if let Some(previous) = &previous_oid
            && previous.as_bytes() >= oid.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "multi-pack-index OIDL object ids are not strictly sorted".into(),
            ));
        }
        counts[oid.as_bytes()[0] as usize] = counts[oid.as_bytes()[0] as usize]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index fanout overflow".into()))?;
        previous_oid = Some(oid);
        ids.push(oid);
    }

    let mut running = 0u32;
    for (idx, count) in counts.iter().enumerate() {
        running = running
            .checked_add(*count)
            .ok_or_else(|| GitError::InvalidFormat("multi-pack-index fanout overflow".into()))?;
        if fanout[idx] != running {
            return Err(GitError::InvalidFormat(
                "multi-pack-index OIDF fanout does not match OIDL".into(),
            ));
        }
    }
    Ok(ids)
}

pub(crate) fn parse_midx_object_offsets(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
    object_ids: Vec<ObjectId>,
    pack_count: u32,
) -> Result<Vec<MultiPackIndexEntry>> {
    let data = midx_chunk_data(bytes, chunks, *b"OOFF", true)?
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OOFF chunk".into()))?;
    let expected_len = object_ids
        .len()
        .checked_mul(8)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index OOFF chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "error: multi-pack-index object offset chunk is the wrong size\nfatal: multi-pack-index required object offsets chunk missing or corrupted".into(),
        ));
    }
    let large_offsets = midx_chunk_data(bytes, chunks, *b"LOFF", false)?;
    if let Some(large_offsets) = large_offsets
        && large_offsets.len() % 8 != 0
    {
        return Err(GitError::InvalidFormat(
            "multi-pack-index LOFF chunk has invalid length".into(),
        ));
    }

    let mut entries = Vec::with_capacity(object_ids.len());
    for (idx, oid) in object_ids.into_iter().enumerate() {
        let start = idx * 8;
        let pack_int_id = u32_be(&data[start..start + 4]);
        if pack_int_id >= pack_count {
            return Err(GitError::InvalidFormat(
                "multi-pack-index object points past pack table".into(),
            ));
        }
        let raw_offset = u32_be(&data[start + 4..start + 8]);
        let offset = if raw_offset & 0x8000_0000 == 0 {
            u64::from(raw_offset)
        } else {
            let Some(large_offsets) = large_offsets else {
                return Err(GitError::InvalidFormat(
                    "multi-pack-index large offset missing LOFF chunk".into(),
                ));
            };
            let large_idx = (raw_offset & 0x7fff_ffff) as usize;
            let large_start = large_idx.checked_mul(8).ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index LOFF index overflow".into())
            })?;
            let large_end = large_start.checked_add(8).ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index LOFF index overflow".into())
            })?;
            if large_end > large_offsets.len() {
                return Err(GitError::InvalidFormat(
                    "fatal: multi-pack-index large offset out of bounds".into(),
                ));
            }
            u64_be(&large_offsets[large_start..large_end])
        };
        entries.push(MultiPackIndexEntry {
            oid,
            pack_int_id,
            offset,
            force_large_offset: raw_offset & 0x8000_0000 != 0,
        });
    }
    Ok(entries)
}

pub(crate) fn parse_midx_reverse_index(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
    object_count: usize,
) -> Result<Option<Vec<u32>>> {
    let Some(data) = midx_chunk_data(bytes, chunks, *b"RIDX", false)? else {
        return Ok(None);
    };
    let expected_len = object_count
        .checked_mul(4)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index RIDX chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "multi-pack-index reverse-index chunk is the wrong size".into(),
        ));
    }
    let mut positions = Vec::with_capacity(object_count);
    for idx in 0..object_count {
        let start = idx * 4;
        positions.push(u32_be(&data[start..start + 4]));
    }
    validate_position_permutation(&positions)?;
    Ok(Some(positions))
}

pub(crate) fn parse_midx_bitmapped_packs(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
    pack_count: usize,
    object_count: usize,
) -> Result<Option<Vec<MultiPackBitmapPack>>> {
    let Some(data) = midx_chunk_data(bytes, chunks, *b"BTMP", false)? else {
        return Ok(None);
    };
    let expected_len = pack_count
        .checked_mul(8)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index BTMP chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "multi-pack-index BTMP chunk has invalid length".into(),
        ));
    }
    let mut entries = Vec::with_capacity(pack_count);
    for idx in 0..pack_count {
        let start = idx * 8;
        let bitmap_pos = u32_be(&data[start..start + 4]);
        let bitmap_nr = u32_be(&data[start + 4..start + 8]);
        let bitmap_end = u64::from(bitmap_pos)
            .checked_add(u64::from(bitmap_nr))
            .ok_or_else(|| {
                GitError::InvalidFormat("multi-pack-index BTMP range overflow".into())
            })?;
        if bitmap_end > object_count as u64 {
            return Err(GitError::InvalidFormat(
                "multi-pack-index BTMP range points past object table".into(),
            ));
        }
        entries.push(MultiPackBitmapPack {
            bitmap_pos,
            bitmap_nr,
        });
    }
    Ok(Some(entries))
}

pub(crate) fn midx_chunk_data<'a>(
    bytes: &'a [u8],
    chunks: &[MultiPackIndexChunk],
    id: [u8; 4],
    required: bool,
) -> Result<Option<&'a [u8]>> {
    let Some(chunk) = chunks.iter().find(|chunk| chunk.id == id) else {
        if required {
            return Err(GitError::InvalidFormat(format!(
                "multi-pack-index missing {} chunk",
                std::str::from_utf8(&id).unwrap_or("required")
            )));
        }
        return Ok(None);
    };
    let start = usize::try_from(chunk.offset)
        .map_err(|_| GitError::InvalidFormat("multi-pack-index chunk offset overflow".into()))?;
    let len = usize::try_from(chunk.len)
        .map_err(|_| GitError::InvalidFormat("multi-pack-index chunk length overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index chunk range overflow".into()))?;
    let Some(data) = bytes.get(start..end) else {
        return Err(GitError::InvalidFormat(
            "multi-pack-index chunk extends past file".into(),
        ));
    };
    Ok(Some(data))
}

pub(crate) fn hash_function_id(format: ObjectFormat) -> u32 {
    match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    }
}

/// Maximum number of clean (run) words that a single EWAH running-length word
/// can describe. The field is 32 bits wide (bits 1..=32 of the RLW).
pub(crate) const EWAH_MAX_RUNNING_LEN: u64 = 0xffff_ffff;

/// Maximum number of literal (dirty) words that can trail a single EWAH
/// running-length word. The field is 31 bits wide (bits 33..=63 of the RLW).
pub(crate) const EWAH_MAX_LITERAL_LEN: u64 = 0x7fff_ffff;

/// All-ones 64-bit word, used to recognise a "clean" run of set bits.
pub(crate) const EWAH_ALL_ONES: u64 = u64::MAX;

impl EwahBitmap {
    /// Constructs an [`EwahBitmap`] in git's canonical EWAH compressed form
    /// from a slice of raw uncompressed 64-bit words.
    ///
    /// Within each word bit `i` corresponds to position `word_index * 64 + i`,
    /// matching git's on-disk convention. `bit_size` records the number of
    /// logical bits the bitmap spans; it must not exceed `words.len() * 64`.
    ///
    /// This mirrors libgit's `ewah_add`/`ewah_add_empty_words` incremental
    /// encoder: consecutive all-zero or all-one words collapse into a run, and
    /// any other word is stored verbatim as a literal. Only the first
    /// `bit_size.div_ceil(64)` words back the declared bits; any extra trailing
    /// words supplied by the caller are ignored, just as git encodes a bitmap
    /// sized to its highest set bit.
    pub fn from_words(bit_size: u32, words: &[u64]) -> Result<Self> {
        let required_words = bit_size.div_ceil(64) as usize;
        if required_words > words.len() {
            return Err(GitError::InvalidFormat(format!(
                "EWAH bit_size {bit_size} requires {required_words} words but only {} supplied",
                words.len()
            )));
        }
        // Only the words that actually back the declared bits matter; libgit
        // never emits clean trailing zero words for the unused tail.
        let significant = &words[..required_words];
        let mut builder = EwahBuilder::new(bit_size);
        for &word in significant {
            if word == 0 {
                builder.add_empty_words(false, 1);
            } else if word == EWAH_ALL_ONES {
                builder.add_empty_words(true, 1);
            } else {
                builder.add_literal(word);
            }
        }
        builder.finish()
    }

    /// Constructs an [`EwahBitmap`] from a set of bit positions.
    ///
    /// `bit_size` is the number of logical bits (typically the pack object
    /// count). Every position in `positions` must be strictly less than
    /// `bit_size`. Positions may be given in any order and may repeat.
    pub fn from_positions(bit_size: u32, positions: &[u32]) -> Result<Self> {
        let word_count = bit_size.div_ceil(64) as usize;
        let mut words = vec![0u64; word_count];
        for &position in positions {
            if position >= bit_size {
                return Err(GitError::InvalidFormat(format!(
                    "EWAH bit position {position} out of range for bit_size {bit_size}"
                )));
            }
            let word_index = (position / 64) as usize;
            let bit_index = position % 64;
            words[word_index] |= 1u64 << bit_index;
        }
        Self::from_words(bit_size, &words)
    }

    /// An empty EWAH bitmap (no bits, no words). This is what git writes for an
    /// all-zero type bitmap (e.g. when a pack has no tags).
    pub fn empty() -> Self {
        Self {
            bit_size: 0,
            words: Vec::new(),
            rlw_position: 0,
        }
    }

    /// Decodes the compressed EWAH back into raw 64-bit words, LSB-first within
    /// each word. The returned vector has `bit_size.div_ceil(64)` entries.
    ///
    /// This is the inverse of [`EwahBitmap::from_words`] for the bits the
    /// bitmap actually covers and is primarily used to validate roundtrips.
    pub fn to_words(&self) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        let mut word_idx = 0usize;
        while word_idx < self.words.len() {
            let rlw = self.words[word_idx];
            let run_bit = rlw & 1;
            let run_words = (rlw >> 1) & EWAH_MAX_RUNNING_LEN;
            let literal_words = (rlw >> 33) as usize;
            word_idx += 1;
            let fill = if run_bit == 1 { EWAH_ALL_ONES } else { 0 };
            for _ in 0..run_words {
                out.push(fill);
            }
            let literal_end = word_idx
                .checked_add(literal_words)
                .filter(|end| *end <= self.words.len())
                .ok_or_else(|| {
                    GitError::InvalidFormat("EWAH literal words extend past word table".into())
                })?;
            out.extend_from_slice(&self.words[word_idx..literal_end]);
            word_idx = literal_end;
        }
        let required_words = (self.bit_size as usize).div_ceil(64);
        if out.len() < required_words {
            out.resize(required_words, 0);
        }
        out.truncate(required_words);
        Ok(out)
    }

    /// Returns the sorted set bit positions covered by this bitmap.
    pub fn to_positions(&self) -> Result<Vec<u32>> {
        let words = self.to_words()?;
        let mut positions = Vec::new();
        for (word_index, word) in words.iter().enumerate() {
            let mut remaining = *word;
            while remaining != 0 {
                let bit = remaining.trailing_zeros();
                let position = (word_index as u64) * 64 + u64::from(bit);
                if position < u64::from(self.bit_size) {
                    // position always fits in u32 because bit_size is u32.
                    positions.push(position as u32);
                }
                remaining &= remaining - 1;
            }
        }
        Ok(positions)
    }

    /// Serialises the bitmap to git's on-disk EWAH byte layout: `bit_size`
    /// (u32 BE), word count (u32 BE), each compressed word (u64 BE), then the
    /// running-length-word position (u32 BE).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.words.len() * 8);
        self.append_bytes(&mut out);
        out
    }

    pub(crate) fn append_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bit_size.to_be_bytes());
        out.extend_from_slice(&(self.words.len() as u32).to_be_bytes());
        for word in &self.words {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&self.rlw_position.to_be_bytes());
    }
}

/// Incremental EWAH compressed-buffer builder mirroring libgit's `ewah_add`.
///
/// The buffer is a sequence of blocks. Each block begins with a running-length
/// word (RLW) and is followed by zero or more literal words:
///   * bit 0      => value of the clean run words (0 or 1)
///   * bits 1..=32 => number of clean run words (32-bit field)
///   * bits 33..=63 => number of trailing literal words (31-bit field)
pub(crate) struct EwahBuilder {
    bit_size: u32,
    words: Vec<u64>,
    rlw_position: usize,
}

impl EwahBuilder {
    pub(crate) fn new(bit_size: u32) -> Self {
        // Every EWAH buffer begins with an RLW, even an empty one.
        Self {
            bit_size,
            words: vec![0u64],
            rlw_position: 0,
        }
    }

    pub(crate) fn rlw(&self) -> u64 {
        self.words[self.rlw_position]
    }

    pub(crate) fn set_rlw(&mut self, value: u64) {
        self.words[self.rlw_position] = value;
    }

    pub(crate) fn rlw_running_len(&self) -> u64 {
        (self.rlw() >> 1) & EWAH_MAX_RUNNING_LEN
    }

    pub(crate) fn rlw_running_bit(&self) -> bool {
        self.rlw() & 1 == 1
    }

    pub(crate) fn rlw_literal_len(&self) -> u64 {
        self.rlw() >> 33
    }

    pub(crate) fn set_running_bit(&mut self, bit: bool) {
        let mut value = self.rlw();
        value &= !1;
        value |= u64::from(bit);
        self.set_rlw(value);
    }

    pub(crate) fn set_running_len(&mut self, len: u64) {
        let mut value = self.rlw();
        value &= !(EWAH_MAX_RUNNING_LEN << 1);
        value |= (len & EWAH_MAX_RUNNING_LEN) << 1;
        self.set_rlw(value);
    }

    pub(crate) fn set_literal_len(&mut self, len: u64) {
        let mut value = self.rlw();
        value &= (1u64 << 33) - 1;
        value |= (len & EWAH_MAX_LITERAL_LEN) << 33;
        self.set_rlw(value);
    }

    /// Begins a fresh RLW block at the end of the buffer.
    pub(crate) fn push_rlw(&mut self) {
        self.rlw_position = self.words.len();
        self.words.push(0);
    }

    /// Appends `number` clean words whose bits are all `value`, mirroring
    /// libgit's `ewah_add_empty_words`.
    ///
    /// A run can only be merged into the current RLW when that RLW has not yet
    /// emitted any literal words and its run either is empty or already carries
    /// the same fill value. Otherwise a fresh RLW block must be started, because
    /// every block stores its run strictly before its literals.
    pub(crate) fn add_empty_words(&mut self, value: bool, mut number: u64) {
        while number > 0 {
            // The current RLW can absorb more run words only when it has no
            // literals yet, its run is either empty or already the right fill
            // value, and the 32-bit run-length field is not already saturated.
            let can_extend = self.rlw_literal_len() == 0
                && (self.rlw_running_len() == 0 || self.rlw_running_bit() == value)
                && self.rlw_running_len() < EWAH_MAX_RUNNING_LEN;
            if !can_extend {
                self.push_rlw();
            }
            if self.rlw_running_len() == 0 {
                self.set_running_bit(value);
            }
            let available = EWAH_MAX_RUNNING_LEN - self.rlw_running_len();
            let take = available.min(number);
            self.set_running_len(self.rlw_running_len() + take);
            number -= take;
        }
    }

    /// Appends a single literal (dirty) word verbatim, mirroring libgit's
    /// `ewah_add_dirty_words` for a count of one.
    pub(crate) fn add_literal(&mut self, word: u64) {
        if self.rlw_literal_len() >= EWAH_MAX_LITERAL_LEN {
            self.push_rlw();
        }
        let literal_len = self.rlw_literal_len();
        self.set_literal_len(literal_len + 1);
        self.words.push(word);
    }

    pub(crate) fn finish(self) -> Result<EwahBitmap> {
        let rlw_position = u32::try_from(self.rlw_position)
            .map_err(|_| GitError::InvalidFormat("EWAH RLW position overflow".into()))?;
        if self.words.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("EWAH word table overflow".into()));
        }
        Ok(EwahBitmap {
            bit_size: self.bit_size,
            words: self.words,
            rlw_position,
        })
    }
}
