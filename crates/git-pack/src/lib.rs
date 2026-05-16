use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use git_core::{GitError, ObjectFormat, ObjectId, Result};
use git_formats::{Bundle, EncodedObject, ObjectType};
use std::collections::HashMap;
use std::io::{Read, Write};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub oid: ObjectId,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStrategy {
    None,
    RefDelta,
    OfsDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackPolicy {
    pub write_bitmaps: bool,
    pub cruft_packs: bool,
    pub geometric_factor: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackFile {
    pub version: u32,
    pub entries: Vec<PackObject>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackObject {
    pub entry: PackEntry,
    pub object: EncodedObject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWrite {
    pub pack: Vec<u8>,
    pub index: Vec<u8>,
    pub checksum: ObjectId,
    pub entries: Vec<PackIndexEntry>,
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
pub struct PackIndexEntry {
    pub oid: ObjectId,
    pub crc32: u32,
    pub offset: u64,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPackIndexEntry {
    pub oid: ObjectId,
    pub pack_int_id: u32,
    pub offset: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
    OfsDelta,
    RefDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedPackEntry {
    Resolved(PackObject),
    Delta {
        base: DeltaBase,
        compressed_size: u64,
        delta_size: u64,
        offset: u64,
        delta: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeltaBase {
    Offset(u64),
    Ref(ObjectId),
}

impl PackFile {
    pub fn parse_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        Self::parse_with_base(bytes, format, |_| Ok(None))
    }

    pub fn parse_bundle(bundle: &Bundle) -> Result<Self> {
        Self::parse(&bundle.pack, bundle.format)
    }

    pub fn parse_thin<F>(bytes: &[u8], format: ObjectFormat, external_base: F) -> Result<Self>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        Self::parse_with_base(bytes, format, external_base)
    }

    fn parse_with_base<F>(bytes: &[u8], format: ObjectFormat, mut external_base: F) -> Result<Self>
    where
        F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
    {
        let trailer_len = format.raw_len();
        if bytes.len() < 12 + trailer_len {
            return Err(GitError::InvalidFormat("pack file too short".into()));
        }
        let trailer_offset = bytes.len() - trailer_len;
        let checksum = git_core::digest_bytes(format, &bytes[..trailer_offset])?;
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
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_offset = offset;
            let header = parse_entry_header(bytes, &mut offset)?;
            let base =
                match header.kind {
                    PackObjectKind::OfsDelta => Some(DeltaBase::Offset(
                        parse_ofs_delta_base_offset(bytes, &mut offset, entry_offset as u64)?,
                    )),
                    PackObjectKind::RefDelta => {
                        let hash_len = format.raw_len();
                        if offset + hash_len > trailer_offset {
                            return Err(GitError::InvalidFormat(
                                "truncated ref-delta base object id".into(),
                            ));
                        }
                        let oid = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;
                        offset += hash_len;
                        Some(DeltaBase::Ref(oid))
                    }
                    _ => None,
                };
            let mut decoder = ZlibDecoder::new(&bytes[offset..trailer_offset]);
            let mut body = Vec::with_capacity(header.size.min(usize::MAX as u64) as usize);
            decoder.read_to_end(&mut body)?;
            if body.len() as u64 != header.size {
                return Err(GitError::InvalidObject(format!(
                    "pack object declared {} bytes, decoded {}",
                    header.size,
                    body.len()
                )));
            }
            let consumed = decoder.total_in() as usize;
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
            entries: resolve_pack_entries(entries, format, &mut external_base)?,
            checksum,
        })
    }

    pub fn write_undeltified_sha1(objects: &[EncodedObject]) -> Result<PackWrite> {
        Self::write_undeltified(objects, ObjectFormat::Sha1)
    }

    pub fn write_undeltified(objects: &[EncodedObject], format: ObjectFormat) -> Result<PackWrite> {
        Self::write_with_delta_strategy(objects, format, DeltaStrategy::None)
    }

    pub fn write_with_delta_strategy(
        objects: &[EncodedObject],
        format: ObjectFormat,
        delta_strategy: DeltaStrategy,
    ) -> Result<PackWrite> {
        if objects.len() > u32::MAX as usize {
            return Err(GitError::InvalidFormat("too many pack objects".into()));
        }
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
        let mut index_entries = Vec::with_capacity(objects.len());
        let mut written_offsets: Vec<usize> = Vec::with_capacity(objects.len());
        let mut object_ids: Vec<ObjectId> = Vec::with_capacity(objects.len());
        for (idx, object) in objects.iter().enumerate() {
            let offset = pack.len();
            if offset > u64::MAX as usize {
                return Err(GitError::InvalidFormat("pack offset overflow".into()));
            }
            let oid = object.object_id(format)?;
            let mut entry_bytes = Vec::new();
            let delta_base = idx.checked_sub(1).filter(|_| {
                delta_strategy != DeltaStrategy::None
                    && objects[idx - 1].object_type == object.object_type
            });
            if let Some(base_idx) = delta_base {
                let base = &objects[base_idx];
                let delta = create_pack_delta(&base.body, &object.body);
                if delta.len() < object.body.len() {
                    match delta_strategy {
                        DeltaStrategy::None => unreachable!(),
                        DeltaStrategy::RefDelta => {
                            write_pack_entry_header_kind(&mut entry_bytes, 7, delta.len() as u64);
                            entry_bytes.extend_from_slice(object_ids[base_idx].as_bytes());
                        }
                        DeltaStrategy::OfsDelta => {
                            write_pack_entry_header_kind(&mut entry_bytes, 6, delta.len() as u64);
                            let relative = offset
                                .checked_sub(written_offsets[base_idx])
                                .ok_or_else(|| {
                                    GitError::InvalidFormat(
                                        "ofs-delta base offset is after delta".into(),
                                    )
                                })?;
                            write_ofs_delta_offset(&mut entry_bytes, relative as u64)?;
                        }
                    }
                    write_compressed_payload(&mut entry_bytes, &delta)?;
                } else {
                    write_undeltified_entry(&mut entry_bytes, object)?;
                }
            } else {
                write_undeltified_entry(&mut entry_bytes, object)?;
            }
            let crc32 = crc32fast::hash(&entry_bytes);
            pack.extend_from_slice(&entry_bytes);
            index_entries.push(PackIndexEntry {
                oid: oid.clone(),
                crc32,
                offset: offset as u64,
            });
            written_offsets.push(offset);
            object_ids.push(oid);
        }
        let checksum = git_core::digest_bytes(format, &pack)?;
        pack.extend_from_slice(checksum.as_bytes());
        let index = PackIndex::write_v2(format, &index_entries, &checksum)?;
        Ok(PackWrite {
            pack,
            index,
            checksum,
            entries: index_entries,
        })
    }
}

impl PackIndex {
    pub fn parse_v2_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        if bytes[..4] != [0xff, b't', b'O', b'c'] {
            return Self::parse_v1(bytes, format);
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
        let actual_index_checksum =
            git_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat(format!(
                "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
            )));
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
        let large_offset_table = checked_range(offset, large_offset_count, 8, bytes.len())?;
        offset = large_offset_table.end;

        let expected_trailer_offset = bytes.len() - hash_len * 2;
        if offset != expected_trailer_offset {
            return Err(GitError::InvalidFormat(format!(
                "pack index has {} unexpected bytes before trailer",
                expected_trailer_offset.saturating_sub(offset)
            )));
        }
        let pack_checksum = ObjectId::from_raw(format, &bytes[offset..offset + hash_len])?;

        let mut entries = Vec::with_capacity(count);
        for idx in 0..count {
            let oid_start = oid_table.start + idx * hash_len;
            let crc_start = crc_table.start + idx * 4;
            let offset_start = small_offset_table.start + idx * 4;
            let raw_offset = u32_be(&bytes[offset_start..offset_start + 4]);
            let offset = if raw_offset & 0x8000_0000 == 0 {
                u64::from(raw_offset)
            } else {
                let large_idx = (raw_offset & 0x7fff_ffff) as usize;
                let large_start = large_offset_table.start + large_idx * 8;
                if large_idx >= large_offset_count {
                    return Err(GitError::InvalidFormat(
                        "pack index large offset points past table".into(),
                    ));
                }
                u64_be(&bytes[large_start..large_start + 8])
            };
            entries.push(PackIndexEntry {
                oid: ObjectId::from_raw(format, &bytes[oid_start..oid_start + hash_len])?,
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

    fn parse_v1(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 256 * 4 + 2 * hash_len {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let index_checksum_offset = bytes.len() - hash_len;
        let actual_index_checksum =
            git_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat(format!(
                "pack index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
            )));
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
                && previous.as_bytes() >= oid.as_bytes()
            {
                return Err(GitError::InvalidFormat(
                    "pack index object ids are not strictly sorted".into(),
                ));
            }
            previous_oid = Some(oid.clone());
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
        let mut entries = entries.to_vec();
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
        let index_checksum = git_core::digest_bytes(format, &index)?;
        index.extend_from_slice(index_checksum.as_bytes());
        Ok(index)
    }
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
        let checksum = git_core::digest_bytes(format, &out)?;
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
            return Err(GitError::InvalidFormat(
                "missing reverse index signature".into(),
            ));
        }
        let version = u32_be(&bytes[4..8]);
        if version != 1 {
            return Err(GitError::Unsupported(format!(
                "reverse index version {version}"
            )));
        }
        let hash_id = u32_be(&bytes[8..12]);
        if hash_id != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "reverse index hash id {hash_id} does not match {}",
                format.name()
            )));
        }

        let index_checksum_offset = bytes.len() - hash_len;
        let actual_index_checksum =
            git_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
        let index_checksum = ObjectId::from_raw(format, &bytes[index_checksum_offset..])?;
        if actual_index_checksum != index_checksum {
            return Err(GitError::InvalidFormat(format!(
                "reverse index checksum mismatch: expected {index_checksum}, got {actual_index_checksum}"
            )));
        }

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

        Ok(Self {
            version,
            format,
            positions,
            pack_checksum,
            index_checksum,
        })
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
        let checksum = git_core::digest_bytes(format, &out)?;
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
            git_core::digest_bytes(format, &bytes[..index_checksum_offset])?;
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

impl MultiPackIndex {
    pub fn write(
        format: ObjectFormat,
        version: u8,
        pack_names: &[String],
        objects: &[MultiPackIndexEntry],
    ) -> Result<Vec<u8>> {
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
        validate_midx_pack_names(pack_names)?;
        if version == 1 && pack_names.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(GitError::InvalidFormat(
                "multi-pack-index v1 pack names must be sorted".into(),
            ));
        }

        let mut objects = objects.to_vec();
        objects.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        let mut previous_oid: Option<ObjectId> = None;
        for object in &objects {
            if object.oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "multi-pack-index object format does not match index format".into(),
                ));
            }
            if let Some(previous) = &previous_oid
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
            previous_oid = Some(object.oid.clone());
        }

        let object_ids: Vec<ObjectId> = objects.iter().map(|entry| entry.oid.clone()).collect();
        let mut large_offsets = Vec::new();
        let mut chunks = vec![
            (*b"PNAM", write_midx_pack_names(pack_names)),
            (*b"OIDF", write_midx_oid_fanout(&object_ids)?),
            (*b"OIDL", write_midx_oid_lookup(&object_ids)),
            (
                *b"OOFF",
                write_midx_object_offsets(&objects, &mut large_offsets)?,
            ),
        ];
        if !large_offsets.is_empty() {
            chunks.push((*b"LOFF", large_offsets));
        }
        write_multi_pack_index_chunks(format, version, pack_names.len() as u32, &chunks)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
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

        let actual_checksum = git_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "multi-pack-index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
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

fn validate_midx_pack_names(pack_names: &[String]) -> Result<()> {
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

fn write_midx_pack_names(pack_names: &[String]) -> Vec<u8> {
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

fn write_midx_oid_fanout(object_ids: &[ObjectId]) -> Result<Vec<u8>> {
    let mut counts = [0u32; 256];
    for oid in object_ids {
        let first = oid.as_bytes()[0] as usize;
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

fn write_midx_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
    let mut out = Vec::new();
    for oid in object_ids {
        out.extend_from_slice(oid.as_bytes());
    }
    out
}

fn write_midx_object_offsets(
    objects: &[MultiPackIndexEntry],
    large_offsets: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for object in objects {
        out.extend_from_slice(&object.pack_int_id.to_be_bytes());
        if object.offset < 0x8000_0000 {
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

fn write_multi_pack_index_chunks(
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
    let checksum = git_core::digest_bytes(format, &out)?;
    out.extend_from_slice(checksum.as_bytes());
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntryHeader {
    kind: PackObjectKind,
    size: u64,
}

fn parse_entry_header(bytes: &[u8], offset: &mut usize) -> Result<EntryHeader> {
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

fn parse_ofs_delta_base_offset(bytes: &[u8], offset: &mut usize, entry_offset: u64) -> Result<u64> {
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

fn resolve_pack_entries<F>(
    parsed: Vec<ParsedPackEntry>,
    format: ObjectFormat,
    external_base: &mut F,
) -> Result<Vec<PackObject>>
where
    F: FnMut(&ObjectId) -> Result<Option<EncodedObject>>,
{
    let mut offset_to_index = HashMap::with_capacity(parsed.len());
    for (idx, entry) in parsed.iter().enumerate() {
        offset_to_index.insert(parsed_entry_offset(entry), idx);
    }

    let mut resolved = vec![None; parsed.len()];
    let mut oid_to_index = HashMap::new();
    let mut unresolved = 0usize;
    for (idx, entry) in parsed.iter().enumerate() {
        match entry {
            ParsedPackEntry::Resolved(object) => {
                oid_to_index.insert(object.entry.oid.clone(), idx);
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
            let body = apply_pack_delta(&base_object.body, delta)?;
            let object = EncodedObject::new(base_object.object_type, body);
            let oid = object.object_id(format)?;
            let pack_object = PackObject {
                entry: PackEntry {
                    oid: oid.clone(),
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

fn parsed_entry_offset(entry: &ParsedPackEntry) -> u64 {
    match entry {
        ParsedPackEntry::Resolved(object) => object.entry.offset,
        ParsedPackEntry::Delta { offset, .. } => *offset,
    }
}

fn delta_base_object<F>(
    base: &DeltaBase,
    offset_to_index: &HashMap<u64, usize>,
    oid_to_index: &HashMap<ObjectId, usize>,
    resolved: &[Option<PackObject>],
    external_base: &mut F,
) -> Result<Option<EncodedObject>>
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
            Ok(resolved[index].as_ref().map(|object| object.object.clone()))
        }
        DeltaBase::Ref(oid) => {
            if let Some(index) = oid_to_index.get(oid).copied() {
                return Ok(resolved[index].as_ref().map(|object| object.object.clone()));
            }
            external_base(oid)
        }
    }
}

fn apply_pack_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut cursor = 0usize;
    let base_size = read_delta_varint(delta, &mut cursor)?;
    if base_size != base.len() as u64 {
        return Err(GitError::InvalidObject(format!(
            "delta base size mismatch: expected {base_size}, got {}",
            base.len()
        )));
    }
    let result_size = read_delta_varint(delta, &mut cursor)?;
    let mut result = Vec::with_capacity(result_size.min(usize::MAX as u64) as usize);
    while cursor < delta.len() {
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
            result.extend_from_slice(slice);
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
            result.extend_from_slice(slice);
            cursor = end;
        } else {
            return Err(GitError::InvalidObject(
                "delta contains reserved zero command".into(),
            ));
        }
    }
    if result.len() as u64 != result_size {
        return Err(GitError::InvalidObject(format!(
            "delta result size mismatch: expected {result_size}, got {}",
            result.len()
        )));
    }
    Ok(result)
}

fn decoded_delta_result_size(delta: &[u8]) -> Result<u64> {
    let mut cursor = 0usize;
    let _ = read_delta_varint(delta, &mut cursor)?;
    read_delta_varint(delta, &mut cursor)
}

fn create_pack_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut delta = Vec::new();
    write_delta_varint(&mut delta, base.len() as u64);
    write_delta_varint(&mut delta, target.len() as u64);

    let common_prefix = base
        .iter()
        .zip(target.iter())
        .take_while(|(left, right)| left == right)
        .count();
    write_delta_copy(&mut delta, 0, common_prefix as u64);
    write_delta_insert(&mut delta, &target[common_prefix..]);
    delta
}

fn write_delta_varint(out: &mut Vec<u8>, mut value: u64) {
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

fn write_delta_copy(out: &mut Vec<u8>, mut offset: u64, mut size: u64) {
    while size != 0 {
        let chunk = size.min(0x10000);
        let encoded_size = if chunk == 0x10000 { 0 } else { chunk };
        let mut command = 0x80u8;
        let mut payload = Vec::new();
        for idx in 0..4 {
            let byte = ((offset >> (idx * 8)) & 0xff) as u8;
            if byte != 0 {
                command |= 1 << idx;
                payload.push(byte);
            }
        }
        for idx in 0..3 {
            let byte = ((encoded_size >> (idx * 8)) & 0xff) as u8;
            if byte != 0 {
                command |= 0x10 << idx;
                payload.push(byte);
            }
        }
        out.push(command);
        out.extend_from_slice(&payload);
        offset += chunk;
        size -= chunk;
    }
}

fn write_delta_insert(out: &mut Vec<u8>, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let chunk_len = bytes.len().min(0x7f);
        out.push(chunk_len as u8);
        out.extend_from_slice(&bytes[..chunk_len]);
        bytes = &bytes[chunk_len..];
    }
}

fn read_delta_varint(delta: &[u8], cursor: &mut usize) -> Result<u64> {
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

fn read_delta_copy_value(
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

fn write_undeltified_entry(out: &mut Vec<u8>, object: &EncodedObject) -> Result<()> {
    write_entry_header(out, object.object_type, object.body.len() as u64);
    write_compressed_payload(out, &object.body)
}

fn write_compressed_payload(out: &mut Vec<u8>, body: &[u8]) -> Result<()> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body)?;
    out.extend_from_slice(&encoder.finish()?);
    Ok(())
}

fn write_entry_header(out: &mut Vec<u8>, object_type: ObjectType, size: u64) {
    let type_code = match object_type {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    };
    write_pack_entry_header_kind(out, type_code, size);
}

fn write_pack_entry_header_kind(out: &mut Vec<u8>, type_code: u8, mut size: u64) {
    let mut byte = (type_code << 4) | ((size as u8) & 0x0f);
    size >>= 4;
    if size != 0 {
        byte |= 0x80;
    }
    out.push(byte);
    while size != 0 {
        let mut byte = (size as u8) & 0x7f;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
}

fn write_ofs_delta_offset(out: &mut Vec<u8>, relative: u64) -> Result<()> {
    if relative == 0 {
        return Err(GitError::InvalidFormat(
            "ofs-delta relative offset cannot be zero".into(),
        ));
    }
    let mut value = relative;
    let mut bytes = vec![(value & 0x7f) as u8];
    value >>= 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    bytes.reverse();
    out.extend_from_slice(&bytes);
    Ok(())
}

fn next_byte(bytes: &[u8], offset: &mut usize) -> Result<u8> {
    let Some(byte) = bytes.get(*offset).copied() else {
        return Err(GitError::InvalidFormat(
            "truncated pack entry header".into(),
        ));
    };
    *offset += 1;
    Ok(byte)
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn checked_range(
    start: usize,
    count: usize,
    width: usize,
    total: usize,
) -> Result<std::ops::Range<usize>> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| GitError::InvalidFormat("pack index table overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("pack index table overflow".into()))?;
    if end > total {
        return Err(GitError::InvalidFormat("truncated pack index table".into()));
    }
    Ok(start..end)
}

fn validate_position_permutation(positions: &[u32]) -> Result<()> {
    let mut seen = vec![false; positions.len()];
    for position in positions {
        let idx = *position as usize;
        if idx >= positions.len() {
            return Err(GitError::InvalidFormat(
                "reverse index position points past object table".into(),
            ));
        }
        if seen[idx] {
            return Err(GitError::InvalidFormat(
                "reverse index position is duplicated".into(),
            ));
        }
        seen[idx] = true;
    }
    Ok(())
}

fn parse_midx_pack_names(
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
                "multi-pack-index PNAM entry is unterminated".into(),
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

fn parse_midx_oid_fanout(
    bytes: &[u8],
    chunks: &[MultiPackIndexChunk],
) -> Result<([u32; 256], usize)> {
    let data = midx_chunk_data(bytes, chunks, *b"OIDF", true)?
        .ok_or_else(|| GitError::InvalidFormat("multi-pack-index missing OIDF chunk".into()))?;
    if data.len() != 256 * 4 {
        return Err(GitError::InvalidFormat(
            "multi-pack-index OIDF chunk has invalid length".into(),
        ));
    }
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = u32_be(&data[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "multi-pack-index OIDF fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255] as usize))
}

fn parse_midx_object_ids(
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
            "multi-pack-index OIDL chunk has invalid length".into(),
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
        previous_oid = Some(oid.clone());
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

fn parse_midx_object_offsets(
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
            "multi-pack-index OOFF chunk has invalid length".into(),
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
                    "multi-pack-index large offset points past LOFF chunk".into(),
                ));
            }
            u64_be(&large_offsets[large_start..large_end])
        };
        entries.push(MultiPackIndexEntry {
            oid,
            pack_int_id,
            offset,
        });
    }
    Ok(entries)
}

fn parse_midx_reverse_index(
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
            "multi-pack-index RIDX chunk has invalid length".into(),
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

fn parse_midx_bitmapped_packs(
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

fn midx_chunk_data<'a>(
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

fn hash_function_id(format: ObjectFormat) -> u32 {
    match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    #[test]
    fn parses_single_blob_pack() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"hello\n");
        let parsed = PackFile::parse_sha1(&pack).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.entries.len(), 1);
        let object = &parsed.entries[0].object;
        assert_eq!(object.object_type, ObjectType::Blob);
        assert_eq!(object.body, b"hello\n");
        assert_eq!(
            parsed.entries[0].entry.oid.to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn parses_single_blob_pack_sha256() {
        let pack = single_object_pack(ObjectFormat::Sha256, ObjectType::Blob, b"hello\n");
        let parsed = PackFile::parse(&pack, ObjectFormat::Sha256).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.entries.len(), 1);
        let object = &parsed.entries[0].object;
        assert_eq!(object.object_type, ObjectType::Blob);
        assert_eq!(object.body, b"hello\n");
        assert_eq!(
            parsed.entries[0].entry.oid,
            object.object_id(ObjectFormat::Sha256).unwrap()
        );
    }

    #[test]
    fn parses_bundle_pack_payload_with_bundle_format() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"bundle\n");
        let oid = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"bundle\n").unwrap();
        let bundle_bytes = format!("# v2 git bundle\n{oid} refs/heads/main\n\n")
            .into_bytes()
            .into_iter()
            .chain(pack)
            .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        let parsed = PackFile::parse_bundle(&bundle).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].object.object_type, ObjectType::Blob);
        assert_eq!(parsed.entries[0].object.body, b"bundle\n");
    }

    #[test]
    fn rejects_bundle_pack_payload_with_wrong_object_format() {
        let pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"bundle\n");
        let oid = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"bundle\n").unwrap();
        let bundle_bytes =
            format!("# v3 git bundle\n@object-format=sha256\n{oid} refs/heads/main\n\n")
                .into_bytes()
                .into_iter()
                .chain(pack)
                .collect::<Vec<_>>();
        let bundle = Bundle::parse(&bundle_bytes, ObjectFormat::Sha1).unwrap();

        assert!(PackFile::parse_bundle(&bundle).is_err());
    }

    #[test]
    fn writes_pack_and_index_that_round_trip() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        let written = PackFile::write_undeltified_sha1(std::slice::from_ref(&object)).unwrap();
        let pack = PackFile::parse_sha1(&written.pack).unwrap();
        let index = PackIndex::parse_v2_sha1(&written.index).unwrap();
        let oid = object.object_id(ObjectFormat::Sha1).unwrap();
        assert_eq!(pack.entries[0].object, object);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(index.find(&oid).unwrap().offset, 12);
    }

    #[test]
    fn writes_sha256_pack_and_index_that_round_trip() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello sha256\n".to_vec());
        let written =
            PackFile::write_undeltified(std::slice::from_ref(&object), ObjectFormat::Sha256)
                .unwrap();
        let pack = PackFile::parse(&written.pack, ObjectFormat::Sha256).unwrap();
        let index = PackIndex::parse(&written.index, ObjectFormat::Sha256).unwrap();
        let oid = object.object_id(ObjectFormat::Sha256).unwrap();
        assert_eq!(pack.entries[0].object, object);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(index.pack_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(index.index_checksum.format(), ObjectFormat::Sha256);
        assert_eq!(index.find(&oid).unwrap().offset, 12);
    }

    #[test]
    fn writes_ref_delta_pack_and_index_that_round_trip() {
        let (base, changed) = similar_blob_objects();
        let written = PackFile::write_with_delta_strategy(
            &[base.clone(), changed.clone()],
            ObjectFormat::Sha1,
            DeltaStrategy::RefDelta,
        )
        .unwrap();
        let mut second_offset = written.entries[1].offset as usize;
        let header = parse_entry_header(&written.pack, &mut second_offset).unwrap();
        assert_eq!(header.kind, PackObjectKind::RefDelta);

        let pack = PackFile::parse_sha1(&written.pack).unwrap();
        let index = PackIndex::parse_v2_sha1(&written.index).unwrap();
        let oid = changed.object_id(ObjectFormat::Sha1).unwrap();
        assert_eq!(pack.entries[0].object, base);
        assert_eq!(pack.entries[1].object, changed);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(index.find(&oid).unwrap().offset, written.entries[1].offset);
    }

    #[test]
    fn writes_ofs_delta_pack_and_index_that_round_trip() {
        let (base, changed) = similar_blob_objects();
        let written = PackFile::write_with_delta_strategy(
            &[base.clone(), changed.clone()],
            ObjectFormat::Sha1,
            DeltaStrategy::OfsDelta,
        )
        .unwrap();
        let mut second_offset = written.entries[1].offset as usize;
        let header = parse_entry_header(&written.pack, &mut second_offset).unwrap();
        assert_eq!(header.kind, PackObjectKind::OfsDelta);

        let pack = PackFile::parse_sha1(&written.pack).unwrap();
        let index = PackIndex::parse_v2_sha1(&written.index).unwrap();
        let oid = changed.object_id(ObjectFormat::Sha1).unwrap();
        assert_eq!(pack.entries[0].object, base);
        assert_eq!(pack.entries[1].object, changed);
        assert_eq!(index.pack_checksum, pack.checksum);
        assert_eq!(index.find(&oid).unwrap().offset, written.entries[1].offset);
    }

    #[test]
    fn resolves_ofs_delta_pack_entry() {
        let base = b"hello";
        let result = b"hello world";
        let pack = two_object_delta_pack(ObjectFormat::Sha1, base, result, DeltaKind::Offset);
        let parsed = PackFile::parse_sha1(&pack).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].object.body, base);
        assert_eq!(parsed.entries[1].object.body, result);
        assert_eq!(
            parsed.entries[1].entry.oid,
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result).unwrap()
        );
    }

    #[test]
    fn resolves_ref_delta_pack_entry() {
        let base = b"hello";
        let result = b"hello world";
        let pack = two_object_delta_pack(ObjectFormat::Sha1, base, result, DeltaKind::Ref);
        let parsed = PackFile::parse_sha1(&pack).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].object.body, base);
        assert_eq!(parsed.entries[1].object.body, result);
        assert_eq!(
            parsed.entries[1].entry.oid,
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result).unwrap()
        );
    }

    #[test]
    fn resolves_thin_ref_delta_pack_entry_with_external_base() {
        let base = b"hello";
        let result = b"hello world";
        let pack = thin_ref_delta_pack(ObjectFormat::Sha1, base, result);
        assert!(PackFile::parse_sha1(&pack).is_err());

        let base_oid = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", base).unwrap();
        let parsed = PackFile::parse_thin(&pack, ObjectFormat::Sha1, |oid| {
            if oid == &base_oid {
                Ok(Some(EncodedObject::new(ObjectType::Blob, base.to_vec())))
            } else {
                Ok(None)
            }
        })
        .unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].object.body, result);
        assert_eq!(
            parsed.entries[0].entry.oid,
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", result).unwrap()
        );
    }

    #[test]
    fn rejects_bad_pack_checksum() {
        let mut pack = single_object_pack(ObjectFormat::Sha1, ObjectType::Blob, b"hello\n");
        let last = pack.len() - 1;
        pack[last] ^= 1;
        assert!(PackFile::parse_sha1(&pack).is_err());
    }

    #[test]
    fn parses_single_entry_pack_index() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let index = single_entry_index(
            ObjectFormat::Sha1,
            oid.clone(),
            0x1234_5678,
            12,
            pack_checksum.clone(),
        );
        let parsed = PackIndex::parse_v2_sha1(&index).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.find(&oid).unwrap().offset, 12);
        assert_eq!(parsed.find(&oid).unwrap().crc32, 0x1234_5678);
    }

    #[test]
    fn parses_single_entry_pack_index_v1() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let index = single_entry_index_v1(
            ObjectFormat::Sha1,
            oid.clone(),
            0x1234_5678,
            pack_checksum.clone(),
        );
        let parsed = PackIndex::parse(&index, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.find(&oid).unwrap().offset, 0x1234_5678);
        assert_eq!(parsed.find(&oid).unwrap().crc32, 0);
    }

    #[test]
    fn rejects_bad_pack_index_v1_checksum() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let mut index = single_entry_index_v1(ObjectFormat::Sha1, oid, 12, pack_checksum);
        let last = index.len() - 1;
        index[last] ^= 1;
        assert!(PackIndex::parse(&index, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_pack_reverse_index() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let reverse_index =
            PackReverseIndex::write(ObjectFormat::Sha1, &[2, 0, 1], &pack_checksum).unwrap();
        let parsed = PackReverseIndex::parse(&reverse_index, ObjectFormat::Sha1, 3).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.positions, vec![2, 0, 1]);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(
            PackReverseIndex::write(ObjectFormat::Sha1, &parsed.positions, &parsed.pack_checksum)
                .unwrap(),
            reverse_index
        );
    }

    #[test]
    fn rejects_bad_pack_reverse_index_checksum() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let mut reverse_index =
            PackReverseIndex::write(ObjectFormat::Sha1, &[0], &pack_checksum).unwrap();
        let last = reverse_index.len() - 1;
        reverse_index[last] ^= 1;
        assert!(PackReverseIndex::parse(&reverse_index, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn rejects_bad_pack_reverse_index_positions() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let duplicate = pack_reverse_index(ObjectFormat::Sha1, &[0, 0], pack_checksum.clone());
        assert!(PackReverseIndex::parse(&duplicate, ObjectFormat::Sha1, 2).is_err());
        let out_of_range = pack_reverse_index(ObjectFormat::Sha1, &[0, 2], pack_checksum);
        assert!(PackReverseIndex::parse(&out_of_range, ObjectFormat::Sha1, 2).is_err());
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        assert!(PackReverseIndex::write(ObjectFormat::Sha1, &[0, 0], &pack_checksum).is_err());
        assert!(PackReverseIndex::write(ObjectFormat::Sha1, &[0, 2], &pack_checksum).is_err());
    }

    #[test]
    fn parses_pack_mtimes() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let mtimes = PackMtimes::write(
            ObjectFormat::Sha1,
            &[1, 1_700_000_000, u32::MAX],
            &pack_checksum,
        )
        .unwrap();
        let parsed = PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 3).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.mtimes, vec![1, 1_700_000_000, u32::MAX]);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(
            PackMtimes::write(ObjectFormat::Sha1, &parsed.mtimes, &parsed.pack_checksum).unwrap(),
            mtimes
        );
    }

    #[test]
    fn rejects_bad_pack_mtimes_checksum() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let mut mtimes = PackMtimes::write(ObjectFormat::Sha1, &[1], &pack_checksum).unwrap();
        let last = mtimes.len() - 1;
        mtimes[last] ^= 1;
        assert!(PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn rejects_bad_pack_mtimes_shape() {
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha1, b"pack").unwrap();
        let mtimes = pack_mtimes(ObjectFormat::Sha1, &[1, 2], pack_checksum.clone());
        assert!(PackMtimes::parse(&mtimes, ObjectFormat::Sha1, 1).is_err());

        let mut wrong_hash = pack_mtimes(ObjectFormat::Sha1, &[1], pack_checksum);
        wrong_hash[11] = 2;
        let checksum_offset = wrong_hash.len() - ObjectFormat::Sha1.raw_len();
        let checksum =
            git_core::digest_bytes(ObjectFormat::Sha1, &wrong_hash[..checksum_offset]).unwrap();
        wrong_hash[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(PackMtimes::parse(&wrong_hash, ObjectFormat::Sha1, 1).is_err());
    }

    #[test]
    fn parses_multi_pack_index_header_and_chunk_lookup() {
        let first =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n").unwrap();
        let second =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n").unwrap();
        let chunks = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(first.clone(), 0, 12), (second.clone(), 1, 0x1_0000_0000)],
        );
        let midx = multi_pack_index(ObjectFormat::Sha1, 2, 2, &chunks);
        let parsed = MultiPackIndex::parse(&midx, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.pack_count, 2);
        assert_eq!(parsed.pack_names, vec!["pack-a.idx", "pack-b.idx"]);
        assert_eq!(parsed.object_count, 2);
        assert_eq!(parsed.objects.len(), 2);
        assert_eq!(parsed.find(&first).unwrap().pack_int_id, 0);
        assert_eq!(parsed.find(&first).unwrap().offset, 12);
        assert_eq!(parsed.find(&second).unwrap().pack_int_id, 1);
        assert_eq!(parsed.find(&second).unwrap().offset, 0x1_0000_0000);
        assert_eq!(parsed.reverse_index, None);
        assert_eq!(parsed.bitmapped_packs, None);
        assert_eq!(parsed.chunks.len(), 5);
        assert_eq!(parsed.chunks[0].id, *b"PNAM");
        assert_eq!(parsed.chunks[0].offset, 84);
        assert_eq!(parsed.chunks[0].len, 24);
        assert_eq!(parsed.chunks[1].id, *b"OIDF");
        assert_eq!(parsed.chunks[1].offset, 108);
        assert_eq!(parsed.chunks[1].len, 1024);
    }

    #[test]
    fn rejects_bad_multi_pack_index_checksum() {
        let chunks = midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]);
        let mut midx = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        let last = midx.len() - 1;
        midx[last] ^= 1;
        assert!(MultiPackIndex::parse(&midx, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_multi_pack_index_shape() {
        let chunks = midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]);
        let mut wrong_hash = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        wrong_hash[5] = 2;
        let checksum_offset = wrong_hash.len() - ObjectFormat::Sha1.raw_len();
        let checksum =
            git_core::digest_bytes(ObjectFormat::Sha1, &wrong_hash[..checksum_offset]).unwrap();
        wrong_hash[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&wrong_hash, ObjectFormat::Sha1).is_err());

        let mut missing_terminator = multi_pack_index(ObjectFormat::Sha1, 1, 0, &chunks);
        missing_terminator[12] = b'B';
        let checksum_offset = missing_terminator.len() - ObjectFormat::Sha1.raw_len();
        let checksum =
            git_core::digest_bytes(ObjectFormat::Sha1, &missing_terminator[..checksum_offset])
                .unwrap();
        missing_terminator[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&missing_terminator, ObjectFormat::Sha1).is_err());

        let mut bad_offset = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            0,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, Vec::new(), &[]),
        );
        bad_offset[16..24].copy_from_slice(&0u64.to_be_bytes());
        let checksum_offset = bad_offset.len() - ObjectFormat::Sha1.raw_len();
        let checksum =
            git_core::digest_bytes(ObjectFormat::Sha1, &bad_offset[..checksum_offset]).unwrap();
        bad_offset[checksum_offset..].copy_from_slice(checksum.as_bytes());
        assert!(MultiPackIndex::parse(&bad_offset, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_multi_pack_index_pack_names() {
        let missing = multi_pack_index(ObjectFormat::Sha1, 2, 1, &[]);
        assert!(MultiPackIndex::parse(&missing, ObjectFormat::Sha1).is_err());

        let too_few = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            2,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0".to_vec(), &[]),
        );
        assert!(MultiPackIndex::parse(&too_few, ObjectFormat::Sha1).is_err());

        let bad_padding = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0xxxx".to_vec(), &[]),
        );
        assert!(MultiPackIndex::parse(&bad_padding, ObjectFormat::Sha1).is_err());

        let unsorted_v1 = multi_pack_index(
            ObjectFormat::Sha1,
            1,
            2,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-b.idx\0pack-a.idx\0".to_vec(),
                &[],
            ),
        );
        assert!(MultiPackIndex::parse(&unsorted_v1, ObjectFormat::Sha1).is_err());

        let unsorted_v2 = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            2,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-b.idx\0pack-a.idx\0".to_vec(),
                &[],
            ),
        );
        let parsed = MultiPackIndex::parse(&unsorted_v2, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.pack_names, vec!["pack-b.idx", "pack-a.idx"]);
    }

    #[test]
    fn rejects_bad_multi_pack_index_object_tables() {
        let oid_a = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let oid_b = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();

        let missing_oidf = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &[(*b"PNAM", b"pack-a.idx\0\0".to_vec())],
        );
        assert!(MultiPackIndex::parse(&missing_oidf, ObjectFormat::Sha1).is_err());

        let bad_fanout = vec![
            (*b"PNAM", b"pack-a.idx\0\0".to_vec()),
            (*b"OIDF", vec![0; 256 * 4]),
            (*b"OIDL", oid_a.as_bytes().to_vec()),
            (*b"OOFF", midx_ooff_entries(&[(0, 12)], &mut Vec::new())),
        ];
        let bad_fanout = multi_pack_index(ObjectFormat::Sha1, 2, 1, &bad_fanout);
        assert!(MultiPackIndex::parse(&bad_fanout, ObjectFormat::Sha1).is_err());

        let mut unsorted = Vec::new();
        unsorted.push((*b"PNAM", b"pack-a.idx\0\0".to_vec()));
        unsorted.push((*b"OIDF", midx_oid_fanout(&[oid_a.clone(), oid_b.clone()])));
        let mut oid_lookup = Vec::new();
        oid_lookup.extend_from_slice(oid_b.as_bytes());
        oid_lookup.extend_from_slice(oid_a.as_bytes());
        unsorted.push((*b"OIDL", oid_lookup));
        unsorted.push((
            *b"OOFF",
            midx_ooff_entries(&[(0, 12), (0, 24)], &mut Vec::new()),
        ));
        let unsorted = multi_pack_index(ObjectFormat::Sha1, 2, 1, &unsorted);
        assert!(MultiPackIndex::parse(&unsorted, ObjectFormat::Sha1).is_err());

        let bad_pack = multi_pack_index(
            ObjectFormat::Sha1,
            2,
            1,
            &midx_chunks_with_pack_names(
                ObjectFormat::Sha1,
                b"pack-a.idx\0\0".to_vec(),
                &[(oid_a.clone(), 1, 12)],
            ),
        );
        assert!(MultiPackIndex::parse(&bad_pack, ObjectFormat::Sha1).is_err());

        let mut large_offsets = Vec::new();
        let missing_loff = vec![
            (*b"PNAM", b"pack-a.idx\0\0".to_vec()),
            (*b"OIDF", midx_oid_fanout(std::slice::from_ref(&oid_a))),
            (*b"OIDL", oid_a.as_bytes().to_vec()),
            (
                *b"OOFF",
                midx_ooff_entries(&[(0, 0x1_0000_0000)], &mut large_offsets),
            ),
        ];
        let missing_loff = multi_pack_index(ObjectFormat::Sha1, 2, 1, &missing_loff);
        assert!(MultiPackIndex::parse(&missing_loff, ObjectFormat::Sha1).is_err());

        let mut bad_loff =
            midx_chunks_with_pack_names(ObjectFormat::Sha1, b"pack-a.idx\0\0".to_vec(), &[]);
        bad_loff.push((*b"LOFF", vec![0]));
        let bad_loff = multi_pack_index(ObjectFormat::Sha1, 2, 1, &bad_loff);
        assert!(MultiPackIndex::parse(&bad_loff, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_multi_pack_index_bitmap_chunks() {
        let first =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n").unwrap();
        let second =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n").unwrap();
        let mut chunks = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(first, 0, 12), (second, 1, 24)],
        );
        chunks.push((*b"RIDX", midx_u32_table(&[1, 0])));
        chunks.push((*b"BTMP", midx_bitmap_packs(&[(0, 1), (1, 1)])));
        let midx = multi_pack_index(ObjectFormat::Sha1, 2, 2, &chunks);

        let parsed = MultiPackIndex::parse(&midx, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.reverse_index, Some(vec![1, 0]));
        assert_eq!(
            parsed.bitmapped_packs,
            Some(vec![
                MultiPackBitmapPack {
                    bitmap_pos: 0,
                    bitmap_nr: 1,
                },
                MultiPackBitmapPack {
                    bitmap_pos: 1,
                    bitmap_nr: 1,
                },
            ])
        );
    }

    #[test]
    fn writes_multi_pack_index_that_round_trips() {
        let first =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"first object\n").unwrap();
        let second =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"second object\n").unwrap();
        let bytes = MultiPackIndex::write(
            ObjectFormat::Sha1,
            2,
            &["pack-b.idx".into(), "pack-a.idx".into()],
            &[
                MultiPackIndexEntry {
                    oid: second.clone(),
                    pack_int_id: 0,
                    offset: 0x1_0000_0000,
                },
                MultiPackIndexEntry {
                    oid: first.clone(),
                    pack_int_id: 1,
                    offset: 12,
                },
            ],
        )
        .unwrap();

        let parsed = MultiPackIndex::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_names, vec!["pack-b.idx", "pack-a.idx"]);
        assert_eq!(parsed.object_count, 2);
        assert_eq!(parsed.find(&first).unwrap().pack_int_id, 1);
        assert_eq!(parsed.find(&first).unwrap().offset, 12);
        assert_eq!(parsed.find(&second).unwrap().pack_int_id, 0);
        assert_eq!(parsed.find(&second).unwrap().offset, 0x1_0000_0000);
        assert!(parsed.chunks.iter().any(|chunk| chunk.id == *b"LOFF"));
    }

    #[test]
    fn write_multi_pack_index_rejects_invalid_inputs() {
        let oid = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"object\n").unwrap();
        assert!(MultiPackIndex::write(ObjectFormat::Sha1, 3, &["pack-a.idx".into()], &[]).is_err());
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                1,
                &["pack-b.idx".into(), "pack-a.idx".into()],
                &[],
            )
            .is_err()
        );
        assert!(MultiPackIndex::write(ObjectFormat::Sha1, 2, &["pack/a.idx".into()], &[]).is_err());
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                2,
                &["pack-a.idx".into()],
                &[MultiPackIndexEntry {
                    oid: oid.clone(),
                    pack_int_id: 1,
                    offset: 12,
                }],
            )
            .is_err()
        );
        assert!(
            MultiPackIndex::write(
                ObjectFormat::Sha1,
                2,
                &["pack-a.idx".into()],
                &[
                    MultiPackIndexEntry {
                        oid: oid.clone(),
                        pack_int_id: 0,
                        offset: 12,
                    },
                    MultiPackIndexEntry {
                        oid,
                        pack_int_id: 0,
                        offset: 24,
                    },
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_bad_multi_pack_index_bitmap_chunks() {
        let oid_a = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "1111111111111111111111111111111111111111",
        )
        .unwrap();
        let oid_b = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "2222222222222222222222222222222222222222",
        )
        .unwrap();

        let mut duplicate_ridx = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0\0".to_vec(),
            &[(oid_a.clone(), 0, 12), (oid_b.clone(), 0, 24)],
        );
        duplicate_ridx.push((*b"RIDX", midx_u32_table(&[0, 0])));
        let duplicate_ridx = multi_pack_index(ObjectFormat::Sha1, 2, 1, &duplicate_ridx);
        assert!(MultiPackIndex::parse(&duplicate_ridx, ObjectFormat::Sha1).is_err());

        let mut short_btmp = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0pack-b.idx\0\0\0".to_vec(),
            &[(oid_a.clone(), 0, 12), (oid_b.clone(), 1, 24)],
        );
        short_btmp.push((*b"BTMP", midx_bitmap_packs(&[(0, 1)])));
        let short_btmp = multi_pack_index(ObjectFormat::Sha1, 2, 2, &short_btmp);
        assert!(MultiPackIndex::parse(&short_btmp, ObjectFormat::Sha1).is_err());

        let mut out_of_range_btmp = midx_chunks_with_pack_names(
            ObjectFormat::Sha1,
            b"pack-a.idx\0\0".to_vec(),
            &[(oid_a, 0, 12), (oid_b, 0, 24)],
        );
        out_of_range_btmp.push((*b"BTMP", midx_bitmap_packs(&[(1, 2)])));
        let out_of_range_btmp = multi_pack_index(ObjectFormat::Sha1, 2, 1, &out_of_range_btmp);
        assert!(MultiPackIndex::parse(&out_of_range_btmp, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_single_entry_pack_index_sha256() {
        let oid =
            git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello sha256\n").unwrap();
        let pack_checksum = git_core::digest_bytes(ObjectFormat::Sha256, b"pack").unwrap();
        let index = single_entry_index(
            ObjectFormat::Sha256,
            oid.clone(),
            0x1234_5678,
            12,
            pack_checksum.clone(),
        );
        let parsed = PackIndex::parse(&index, ObjectFormat::Sha256).unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.pack_checksum, pack_checksum);
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.find(&oid).unwrap().offset, 12);
        assert_eq!(parsed.find(&oid).unwrap().crc32, 0x1234_5678);
        assert_eq!(parsed.index_checksum.format(), ObjectFormat::Sha256);
    }

    fn similar_blob_objects() -> (EncodedObject, EncodedObject) {
        let mut base = Vec::new();
        for _ in 0..300 {
            base.extend_from_slice(b"common payload\n");
        }
        base.extend_from_slice(b"base\n");
        let mut changed = Vec::new();
        for _ in 0..300 {
            changed.extend_from_slice(b"common payload\n");
        }
        changed.extend_from_slice(b"changed\n");
        (
            EncodedObject::new(ObjectType::Blob, base),
            EncodedObject::new(ObjectType::Blob, changed),
        )
    }

    fn single_object_pack(format: ObjectFormat, object_type: ObjectType, body: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        write_entry_header(&mut pack, object_type, body.len() as u64);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body).unwrap();
        pack.extend_from_slice(&encoder.finish().unwrap());
        let checksum = git_core::digest_bytes(format, &pack).unwrap();
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    #[derive(Clone, Copy)]
    enum DeltaKind {
        Offset,
        Ref,
    }

    fn two_object_delta_pack(
        format: ObjectFormat,
        base: &[u8],
        result: &[u8],
        delta_kind: DeltaKind,
    ) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());

        let base_offset = pack.len();
        write_entry_header(&mut pack, ObjectType::Blob, base.len() as u64);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(base).unwrap();
        pack.extend_from_slice(&encoder.finish().unwrap());

        let delta = append_suffix_delta(base, result);
        let delta_offset = pack.len();
        write_pack_entry_header_kind(
            &mut pack,
            match delta_kind {
                DeltaKind::Offset => 6,
                DeltaKind::Ref => 7,
            },
            delta.len() as u64,
        );
        match delta_kind {
            DeltaKind::Offset => write_ofs_delta_offset(&mut pack, delta_offset - base_offset),
            DeltaKind::Ref => {
                let base_oid = git_core::object_id_for_bytes(format, "blob", base).unwrap();
                pack.extend_from_slice(base_oid.as_bytes());
            }
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&delta).unwrap();
        pack.extend_from_slice(&encoder.finish().unwrap());

        let checksum = git_core::digest_bytes(format, &pack).unwrap();
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    fn thin_ref_delta_pack(format: ObjectFormat, base: &[u8], result: &[u8]) -> Vec<u8> {
        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());

        let delta = append_suffix_delta(base, result);
        write_pack_entry_header_kind(&mut pack, 7, delta.len() as u64);
        let base_oid = git_core::object_id_for_bytes(format, "blob", base).unwrap();
        pack.extend_from_slice(base_oid.as_bytes());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&delta).unwrap();
        pack.extend_from_slice(&encoder.finish().unwrap());

        let checksum = git_core::digest_bytes(format, &pack).unwrap();
        pack.extend_from_slice(checksum.as_bytes());
        pack
    }

    fn append_suffix_delta(base: &[u8], result: &[u8]) -> Vec<u8> {
        assert!(result.starts_with(base));
        let suffix = &result[base.len()..];
        assert!(base.len() < 0x10000);
        assert!(suffix.len() < 0x80);
        let mut delta = Vec::new();
        write_delta_varint(&mut delta, base.len() as u64);
        write_delta_varint(&mut delta, result.len() as u64);
        delta.push(0x90);
        delta.push(base.len() as u8);
        delta.push(suffix.len() as u8);
        delta.extend_from_slice(suffix);
        delta
    }

    fn write_delta_varint(out: &mut Vec<u8>, mut value: u64) {
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

    fn write_pack_entry_header_kind(out: &mut Vec<u8>, type_code: u8, mut size: u64) {
        let mut byte = (type_code << 4) | ((size as u8) & 0x0f);
        size >>= 4;
        if size != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        while size != 0 {
            let mut byte = (size as u8) & 0x7f;
            size >>= 7;
            if size != 0 {
                byte |= 0x80;
            }
            out.push(byte);
        }
    }

    fn write_ofs_delta_offset(out: &mut Vec<u8>, relative: usize) {
        assert!(relative < 0x80);
        out.push(relative as u8);
    }

    fn single_entry_index(
        format: ObjectFormat,
        oid: ObjectId,
        crc32: u32,
        offset: u32,
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut index = Vec::new();
        index.extend_from_slice(&[0xff, b't', b'O', b'c']);
        index.extend_from_slice(&2u32.to_be_bytes());
        for idx in 0..256 {
            let count = if idx >= usize::from(oid.as_bytes()[0]) {
                1u32
            } else {
                0u32
            };
            index.extend_from_slice(&count.to_be_bytes());
        }
        index.extend_from_slice(oid.as_bytes());
        index.extend_from_slice(&crc32.to_be_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(pack_checksum.as_bytes());
        let checksum = git_core::digest_bytes(format, &index).unwrap();
        index.extend_from_slice(checksum.as_bytes());
        index
    }

    fn single_entry_index_v1(
        format: ObjectFormat,
        oid: ObjectId,
        offset: u32,
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut index = Vec::new();
        for idx in 0..256 {
            let count = if idx >= usize::from(oid.as_bytes()[0]) {
                1u32
            } else {
                0u32
            };
            index.extend_from_slice(&count.to_be_bytes());
        }
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(oid.as_bytes());
        index.extend_from_slice(pack_checksum.as_bytes());
        let checksum = git_core::digest_bytes(format, &index).unwrap();
        index.extend_from_slice(checksum.as_bytes());
        index
    }

    fn pack_reverse_index(
        format: ObjectFormat,
        positions: &[u32],
        pack_checksum: ObjectId,
    ) -> Vec<u8> {
        let mut reverse_index = Vec::new();
        reverse_index.extend_from_slice(b"RIDX");
        reverse_index.extend_from_slice(&1u32.to_be_bytes());
        reverse_index.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for position in positions {
            reverse_index.extend_from_slice(&position.to_be_bytes());
        }
        reverse_index.extend_from_slice(pack_checksum.as_bytes());
        let checksum = git_core::digest_bytes(format, &reverse_index).unwrap();
        reverse_index.extend_from_slice(checksum.as_bytes());
        reverse_index
    }

    fn pack_mtimes(format: ObjectFormat, mtimes: &[u32], pack_checksum: ObjectId) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MTME");
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&hash_function_id(format).to_be_bytes());
        for mtime in mtimes {
            out.extend_from_slice(&mtime.to_be_bytes());
        }
        out.extend_from_slice(pack_checksum.as_bytes());
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn midx_chunks_with_pack_names(
        _format: ObjectFormat,
        pack_names: Vec<u8>,
        entries: &[(ObjectId, u32, u64)],
    ) -> Vec<([u8; 4], Vec<u8>)> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let object_ids: Vec<ObjectId> = entries.iter().map(|entry| entry.0.clone()).collect();
        let mut large_offsets = Vec::new();
        let mut chunks = vec![
            (*b"PNAM", pack_names),
            (*b"OIDF", midx_oid_fanout(&object_ids)),
            (*b"OIDL", midx_oid_lookup(&object_ids)),
            (
                *b"OOFF",
                midx_ooff_entries(
                    &entries
                        .iter()
                        .map(|(_oid, pack_int_id, offset)| (*pack_int_id, *offset))
                        .collect::<Vec<_>>(),
                    &mut large_offsets,
                ),
            ),
        ];
        if !large_offsets.is_empty() {
            chunks.push((*b"LOFF", large_offsets));
        }
        chunks
    }

    fn midx_oid_fanout(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut counts = [0u32; 256];
        for oid in object_ids {
            counts[oid.as_bytes()[0] as usize] += 1;
        }
        let mut running = 0u32;
        let mut out = Vec::new();
        for count in counts {
            running += count;
            out.extend_from_slice(&running.to_be_bytes());
        }
        out
    }

    fn midx_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut out = Vec::new();
        for oid in object_ids {
            out.extend_from_slice(oid.as_bytes());
        }
        out
    }

    fn midx_ooff_entries(entries: &[(u32, u64)], large_offsets: &mut Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        for (pack_int_id, offset) in entries {
            out.extend_from_slice(&pack_int_id.to_be_bytes());
            if *offset < 0x8000_0000 {
                out.extend_from_slice(&(*offset as u32).to_be_bytes());
            } else {
                let large_idx = (large_offsets.len() / 8) as u32;
                out.extend_from_slice(&(0x8000_0000 | large_idx).to_be_bytes());
                large_offsets.extend_from_slice(&offset.to_be_bytes());
            }
        }
        out
    }

    fn midx_u32_table(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn midx_bitmap_packs(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (bitmap_pos, bitmap_nr) in entries {
            out.extend_from_slice(&bitmap_pos.to_be_bytes());
            out.extend_from_slice(&bitmap_nr.to_be_bytes());
        }
        out
    }

    fn multi_pack_index(
        format: ObjectFormat,
        version: u8,
        pack_count: u32,
        chunks: &[([u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"MIDX");
        out.push(version);
        out.push(hash_function_id(format) as u8);
        out.push(chunks.len() as u8);
        out.push(0);
        out.extend_from_slice(&pack_count.to_be_bytes());
        let mut chunk_offset = (12 + lookup_len) as u64;
        for (id, data) in chunks {
            out.extend_from_slice(id);
            out.extend_from_slice(&chunk_offset.to_be_bytes());
            chunk_offset += data.len() as u64;
        }
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        for (_id, data) in chunks {
            out.extend_from_slice(data);
        }
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }
}
