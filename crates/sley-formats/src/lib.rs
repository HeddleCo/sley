//! git-formats — Git's remaining on-disk and wire formats: reftables, the
//! commit-graph, bundles, and repository layout.
//!
//! The object model, configuration system, and index format that used to live
//! here now have dedicated crates: [`sley_object`], [`sley_config`], and
//! [`sley_index`].

use sley_config::{ConfigEntry, ConfigSection, GitConfig};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use std::fs;
use std::path::{Path, PathBuf};

const REFTABLE_MAGIC: &[u8; 4] = b"REFT";
const REFTABLE_MAX_BLOCK_SIZE: u32 = 0x00ff_ffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReftableVersion {
    V1,
    V2,
}

impl ReftableVersion {
    fn number(self) -> u8 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }

    fn header_len(self) -> usize {
        match self {
            Self::V1 => 24,
            Self::V2 => 28,
        }
    }

    fn footer_len(self) -> usize {
        match self {
            Self::V1 => 68,
            Self::V2 => 72,
        }
    }

    fn from_number(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            other => Err(GitError::InvalidFormat(format!(
                "unsupported reftable version {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReftableHeader {
    pub version: ReftableVersion,
    pub block_size: u32,
    pub min_update_index: u64,
    pub max_update_index: u64,
    pub object_format: ObjectFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReftableRefValue {
    Deletion,
    Direct(ObjectId),
    Peeled { target: ObjectId, peeled: ObjectId },
    Symbolic(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReftableRefRecord {
    pub name: String,
    pub update_index: u64,
    pub value: ReftableRefValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reftable {
    pub header: ReftableHeader,
    pub refs: Vec<ReftableRefRecord>,
}

impl Reftable {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let header = parse_reftable_header(bytes)?;
        let footer = parse_reftable_footer(bytes, header.version)?;
        if footer.version != header.version
            || footer.block_size != header.block_size
            || footer.min_update_index != header.min_update_index
            || footer.max_update_index != header.max_update_index
            || footer.object_format != header.object_format
        {
            return Err(GitError::InvalidFormat(
                "reftable footer header does not match file header".into(),
            ));
        }
        let footer_start = bytes.len() - header.version.footer_len();
        if footer.obj_id_len > header.object_format.raw_len() as u8 {
            return Err(GitError::InvalidFormat(
                "reftable object id abbreviation length exceeds hash length".into(),
            ));
        }
        let ref_end = [
            footer.ref_index_position,
            footer.obj_position,
            footer.obj_index_position,
            footer.log_position,
            footer.log_index_position,
            footer_start as u64,
        ]
        .into_iter()
        .filter(|position| *position != 0)
        .min()
        .unwrap_or(footer_start as u64) as usize;
        let mut refs = Vec::new();
        let mut offset = header.version.header_len();
        while offset < ref_end {
            if bytes[offset] == 0 {
                offset += 1;
                continue;
            }
            let block_type = bytes[offset];
            if block_type != b'r' {
                break;
            }
            let block_len = read_u24(bytes, offset + 1)? as usize;
            let block_end = if offset == header.version.header_len() {
                block_len
            } else {
                offset
                    .checked_add(block_len)
                    .ok_or_else(|| GitError::InvalidFormat("reftable block overflow".into()))?
            };
            if block_end > ref_end || block_end > bytes.len() {
                return Err(GitError::InvalidFormat(
                    "reftable ref block extends past section".into(),
                ));
            }
            refs.extend(parse_reftable_ref_block(
                &bytes[offset..block_end],
                offset,
                header,
            )?);
            offset = block_end;
        }
        Ok(Self { header, refs })
    }

    pub fn write_ref_only(
        format: ObjectFormat,
        min_update_index: u64,
        max_update_index: u64,
        refs: &[ReftableRefRecord],
    ) -> Result<Vec<u8>> {
        let version = match format {
            ObjectFormat::Sha1 => ReftableVersion::V1,
            ObjectFormat::Sha256 => ReftableVersion::V2,
        };
        let header = ReftableHeader {
            version,
            block_size: 0,
            min_update_index,
            max_update_index,
            object_format: format,
        };
        let mut refs = refs.to_vec();
        refs.sort_by(|left, right| left.name.cmp(&right.name));
        let mut out = write_reftable_header(header);
        if !refs.is_empty() {
            let block_start = out.len();
            out.push(b'r');
            out.extend_from_slice(&[0, 0, 0]);
            let mut previous_name = Vec::new();
            let mut restart_offsets = Vec::new();
            for record in &refs {
                if record.update_index < min_update_index || record.update_index > max_update_index
                {
                    return Err(GitError::InvalidFormat(format!(
                        "reftable ref {} update index {} outside header bounds",
                        record.name, record.update_index
                    )));
                }
                restart_offsets.push(out.len() as u32);
                write_reftable_ref_record(
                    &mut out,
                    format,
                    min_update_index,
                    &previous_name,
                    0,
                    record,
                )?;
                previous_name = record.name.as_bytes().to_vec();
            }
            for offset in &restart_offsets {
                write_u24(&mut out, *offset)?;
            }
            let restart_count = u16::try_from(restart_offsets.len())
                .map_err(|_| GitError::InvalidFormat("too many reftable restart offsets".into()))?;
            out.extend_from_slice(&restart_count.to_be_bytes());
            let block_len = out.len();
            if block_len > REFTABLE_MAX_BLOCK_SIZE as usize {
                return Err(GitError::InvalidFormat(
                    "reftable ref block exceeds maximum size".into(),
                ));
            }
            write_u24_at(&mut out, block_start + 1, block_len as u32)?;
        }
        out.extend_from_slice(&write_reftable_footer(header, 0, 0, 0, 0, 0)?);
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy)]
struct ReftableFooter {
    version: ReftableVersion,
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    object_format: ObjectFormat,
    ref_index_position: u64,
    obj_position: u64,
    obj_id_len: u8,
    obj_index_position: u64,
    log_position: u64,
    log_index_position: u64,
}

fn parse_reftable_header(bytes: &[u8]) -> Result<ReftableHeader> {
    if bytes.len() < 24 {
        return Err(GitError::InvalidFormat("truncated reftable header".into()));
    }
    if &bytes[..4] != REFTABLE_MAGIC {
        return Err(GitError::InvalidFormat("missing reftable magic".into()));
    }
    let version = ReftableVersion::from_number(bytes[4])?;
    if bytes.len() < version.header_len() {
        return Err(GitError::InvalidFormat("truncated reftable header".into()));
    }
    let block_size = read_u24(bytes, 5)?;
    let min_update_index = read_u64(bytes, 8)?;
    let max_update_index = read_u64(bytes, 16)?;
    let object_format = match version {
        ReftableVersion::V1 => ObjectFormat::Sha1,
        ReftableVersion::V2 => match bytes.get(24..28) {
            Some(b"sha1") => ObjectFormat::Sha1,
            Some(b"s256") => ObjectFormat::Sha256,
            Some(value) => {
                return Err(GitError::InvalidFormat(format!(
                    "unsupported reftable hash id {}",
                    String::from_utf8_lossy(value)
                )));
            }
            None => return Err(GitError::InvalidFormat("truncated reftable hash id".into())),
        },
    };
    Ok(ReftableHeader {
        version,
        block_size,
        min_update_index,
        max_update_index,
        object_format,
    })
}

fn write_reftable_header(header: ReftableHeader) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.version.header_len());
    out.extend_from_slice(REFTABLE_MAGIC);
    out.push(header.version.number());
    write_u24(&mut out, header.block_size).expect("header block size fits u24");
    out.extend_from_slice(&header.min_update_index.to_be_bytes());
    out.extend_from_slice(&header.max_update_index.to_be_bytes());
    if header.version == ReftableVersion::V2 {
        out.extend_from_slice(match header.object_format {
            ObjectFormat::Sha1 => b"sha1",
            ObjectFormat::Sha256 => b"s256",
        });
    }
    out
}

fn parse_reftable_footer(bytes: &[u8], version: ReftableVersion) -> Result<ReftableFooter> {
    let footer_len = version.footer_len();
    if bytes.len() < footer_len {
        return Err(GitError::InvalidFormat("truncated reftable footer".into()));
    }
    let start = bytes.len() - footer_len;
    let crc_start = bytes.len() - 4;
    let expected = read_u32(bytes, crc_start)?;
    let actual = crc32(&bytes[start..crc_start]);
    if expected != actual {
        return Err(GitError::InvalidFormat(format!(
            "reftable footer crc mismatch: expected {expected:08x}, got {actual:08x}"
        )));
    }
    let header = parse_reftable_header(&bytes[start..])?;
    let mut offset = start + version.header_len();
    let ref_index_position = read_u64(bytes, offset)?;
    offset += 8;
    let obj_position_and_len = read_u64(bytes, offset)?;
    offset += 8;
    let obj_index_position = read_u64(bytes, offset)?;
    offset += 8;
    let log_position = read_u64(bytes, offset)?;
    offset += 8;
    let log_index_position = read_u64(bytes, offset)?;
    Ok(ReftableFooter {
        version: header.version,
        block_size: header.block_size,
        min_update_index: header.min_update_index,
        max_update_index: header.max_update_index,
        object_format: header.object_format,
        ref_index_position,
        obj_position: obj_position_and_len >> 5,
        obj_id_len: (obj_position_and_len & 0x1f) as u8,
        obj_index_position,
        log_position,
        log_index_position,
    })
}

fn write_reftable_footer(
    header: ReftableHeader,
    ref_index_position: u64,
    obj_position: u64,
    obj_id_len: u8,
    obj_index_position: u64,
    log_position: u64,
) -> Result<Vec<u8>> {
    if obj_id_len > 31 {
        return Err(GitError::InvalidFormat(
            "reftable object id abbreviation length exceeds 31".into(),
        ));
    }
    let mut out = write_reftable_header(header);
    out.extend_from_slice(&ref_index_position.to_be_bytes());
    out.extend_from_slice(&((obj_position << 5) | u64::from(obj_id_len)).to_be_bytes());
    out.extend_from_slice(&obj_index_position.to_be_bytes());
    out.extend_from_slice(&log_position.to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    Ok(out)
}

fn parse_reftable_ref_block(
    block: &[u8],
    block_start: usize,
    header: ReftableHeader,
) -> Result<Vec<ReftableRefRecord>> {
    if block.len() < 6 || block[0] != b'r' {
        return Err(GitError::InvalidFormat("invalid reftable ref block".into()));
    }
    let restart_count = read_u16(block, block.len() - 2)? as usize;
    if restart_count == 0 {
        return Err(GitError::InvalidFormat(
            "reftable ref block has no restart offsets".into(),
        ));
    }
    let restart_table_start = block
        .len()
        .checked_sub(2 + restart_count * 3)
        .ok_or_else(|| GitError::InvalidFormat("truncated reftable restart table".into()))?;
    let mut restart_offsets = Vec::with_capacity(restart_count);
    for idx in 0..restart_count {
        restart_offsets.push(read_u24(block, restart_table_start + idx * 3)? as usize);
    }
    if restart_offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(GitError::InvalidFormat(
            "reftable restart offsets are not sorted".into(),
        ));
    }
    let mut offset = 4;
    let mut previous_name = Vec::new();
    let mut records = Vec::new();
    while offset < restart_table_start {
        let record_offset = block_start + offset;
        let restart = restart_offsets.contains(&record_offset);
        let record = parse_reftable_ref_record(
            block,
            &mut offset,
            restart_table_start,
            header,
            &previous_name,
            restart,
        )?;
        previous_name = record.name.as_bytes().to_vec();
        records.push(record);
    }
    if offset != restart_table_start {
        return Err(GitError::InvalidFormat(
            "reftable ref block ended inside record".into(),
        ));
    }
    Ok(records)
}

fn parse_reftable_ref_record(
    block: &[u8],
    offset: &mut usize,
    end: usize,
    header: ReftableHeader,
    previous_name: &[u8],
    restart: bool,
) -> Result<ReftableRefRecord> {
    let prefix_len = read_reftable_varint(block, offset, end)? as usize;
    if prefix_len > previous_name.len() {
        return Err(GitError::InvalidFormat(
            "reftable ref prefix exceeds previous name".into(),
        ));
    }
    if restart && prefix_len != 0 {
        return Err(GitError::InvalidFormat(
            "reftable restart record uses prefix compression".into(),
        ));
    }
    let suffix_len_and_type = read_reftable_varint(block, offset, end)?;
    let suffix_len = (suffix_len_and_type >> 3) as usize;
    let value_type = (suffix_len_and_type & 0x7) as u8;
    let suffix_end = offset
        .checked_add(suffix_len)
        .ok_or_else(|| GitError::InvalidFormat("reftable suffix overflow".into()))?;
    if suffix_end > end {
        return Err(GitError::InvalidFormat("truncated reftable suffix".into()));
    }
    let mut name = previous_name[..prefix_len].to_vec();
    name.extend_from_slice(&block[*offset..suffix_end]);
    *offset = suffix_end;
    let update_index_delta = read_reftable_varint(block, offset, end)?;
    let update_index = header
        .min_update_index
        .checked_add(update_index_delta)
        .ok_or_else(|| GitError::InvalidFormat("reftable update index overflow".into()))?;
    let value = match value_type {
        0 => ReftableRefValue::Deletion,
        1 => ReftableRefValue::Direct(read_reftable_oid(block, offset, end, header.object_format)?),
        2 => ReftableRefValue::Peeled {
            target: read_reftable_oid(block, offset, end, header.object_format)?,
            peeled: read_reftable_oid(block, offset, end, header.object_format)?,
        },
        3 => {
            let target_len = read_reftable_varint(block, offset, end)? as usize;
            let target_end = offset.checked_add(target_len).ok_or_else(|| {
                GitError::InvalidFormat("reftable symbolic target overflow".into())
            })?;
            if target_end > end {
                return Err(GitError::InvalidFormat(
                    "truncated reftable symbolic target".into(),
                ));
            }
            let target = std::str::from_utf8(&block[*offset..target_end])
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?
                .to_string();
            *offset = target_end;
            ReftableRefValue::Symbolic(target)
        }
        other => {
            return Err(GitError::InvalidFormat(format!(
                "unsupported reftable ref value type {other}"
            )));
        }
    };
    let name = std::str::from_utf8(&name)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    Ok(ReftableRefRecord {
        name,
        update_index,
        value,
    })
}

fn write_reftable_ref_record(
    out: &mut Vec<u8>,
    format: ObjectFormat,
    min_update_index: u64,
    previous_name: &[u8],
    prefix_len: usize,
    record: &ReftableRefRecord,
) -> Result<()> {
    let name = record.name.as_bytes();
    if prefix_len > previous_name.len() || prefix_len > name.len() {
        return Err(GitError::InvalidFormat(
            "reftable ref prefix exceeds name".into(),
        ));
    }
    let value_type = match &record.value {
        ReftableRefValue::Deletion => 0,
        ReftableRefValue::Direct(_) => 1,
        ReftableRefValue::Peeled { .. } => 2,
        ReftableRefValue::Symbolic(_) => 3,
    };
    write_reftable_varint(out, prefix_len as u64);
    write_reftable_varint(out, (((name.len() - prefix_len) as u64) << 3) | value_type);
    out.extend_from_slice(&name[prefix_len..]);
    write_reftable_varint(out, record.update_index - min_update_index);
    match &record.value {
        ReftableRefValue::Deletion => {}
        ReftableRefValue::Direct(oid) => {
            if oid.format() != format {
                return Err(GitError::InvalidFormat(
                    "reftable direct ref object format mismatch".into(),
                ));
            }
            out.extend_from_slice(oid.as_bytes());
        }
        ReftableRefValue::Peeled { target, peeled } => {
            if target.format() != format || peeled.format() != format {
                return Err(GitError::InvalidFormat(
                    "reftable peeled ref object format mismatch".into(),
                ));
            }
            out.extend_from_slice(target.as_bytes());
            out.extend_from_slice(peeled.as_bytes());
        }
        ReftableRefValue::Symbolic(target) => {
            write_reftable_varint(out, target.len() as u64);
            out.extend_from_slice(target.as_bytes());
        }
    }
    Ok(())
}

fn read_reftable_oid(
    block: &[u8],
    offset: &mut usize,
    end: usize,
    format: ObjectFormat,
) -> Result<ObjectId> {
    let oid_end = offset
        .checked_add(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("reftable object id overflow".into()))?;
    if oid_end > end {
        return Err(GitError::InvalidFormat(
            "truncated reftable object id".into(),
        ));
    }
    let oid = ObjectId::from_raw(format, &block[*offset..oid_end])?;
    *offset = oid_end;
    Ok(oid)
}

fn read_reftable_varint(bytes: &[u8], offset: &mut usize, end: usize) -> Result<u64> {
    if *offset >= end {
        return Err(GitError::InvalidFormat("truncated reftable varint".into()));
    }
    let mut value = u64::from(bytes[*offset] & 0x7f);
    while bytes[*offset] & 0x80 != 0 {
        *offset += 1;
        if *offset >= end {
            return Err(GitError::InvalidFormat("truncated reftable varint".into()));
        }
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .ok_or_else(|| GitError::InvalidFormat("reftable varint overflow".into()))?
            | u64::from(bytes[*offset] & 0x7f);
    }
    *offset += 1;
    Ok(value)
}

fn write_reftable_varint(out: &mut Vec<u8>, mut value: u64) {
    let mut bytes = [0u8; 10];
    let mut pos = bytes.len() - 1;
    bytes[pos] = (value & 0x7f) as u8;
    while value > 0x7f {
        value = (value >> 7) - 1;
        pos -= 1;
        bytes[pos] = ((value & 0x7f) as u8) | 0x80;
    }
    out.extend_from_slice(&bytes[pos..]);
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint16".into()))?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

fn read_u24(bytes: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 3)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint24".into()))?;
    Ok((u32::from(raw[0]) << 16) | (u32::from(raw[1]) << 8) | u32::from(raw[2]))
}

fn write_u24(out: &mut Vec<u8>, value: u32) -> Result<()> {
    if value > REFTABLE_MAX_BLOCK_SIZE {
        return Err(GitError::InvalidFormat(format!(
            "uint24 value {value} exceeds maximum"
        )));
    }
    out.push((value >> 16) as u8);
    out.push((value >> 8) as u8);
    out.push(value as u8);
    Ok(())
}

fn write_u24_at(out: &mut [u8], offset: usize, value: u32) -> Result<()> {
    if value > REFTABLE_MAX_BLOCK_SIZE {
        return Err(GitError::InvalidFormat(format!(
            "uint24 value {value} exceeds maximum"
        )));
    }
    let target = out
        .get_mut(offset..offset + 3)
        .ok_or_else(|| GitError::InvalidFormat("uint24 write is out of bounds".into()))?;
    target[0] = (value >> 16) as u8;
    target[1] = (value >> 8) as u8;
    target[2] = value as u8;
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint32".into()))?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| GitError::InvalidFormat("truncated uint64".into()))?;
    Ok(u64::from_be_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraph {
    pub version: u8,
    pub format: ObjectFormat,
    pub base_graph_count: u8,
    pub fanout: [u32; 256],
    pub commits: Vec<CommitGraphEntry>,
    pub chunks: Vec<CommitGraphChunk>,
    pub base_graphs: Vec<ObjectId>,
    pub bloom_filters: Option<CommitGraphBloomFilters>,
    pub checksum: ObjectId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<u32>,
    pub generation: u32,
    pub commit_time: u64,
    pub corrected_commit_date_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphChunk {
    pub id: [u8; 4],
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphBloomFilters {
    pub hash_version: u32,
    pub hash_count: u32,
    pub bits_per_entry: u32,
    pub filters: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphWriteEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub generation: u32,
    pub commit_time: u64,
}

impl CommitGraph {
    pub fn write(format: ObjectFormat, entries: &[CommitGraphWriteEntry]) -> Result<Vec<u8>> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        validate_commit_graph_write_entries(format, &entries)?;
        let object_ids = entries
            .iter()
            .map(|entry| entry.oid)
            .collect::<Vec<_>>();
        let (cdat, edge) = write_commit_graph_commit_data(&entries)?;
        let mut chunks = vec![
            (*b"OIDF", write_commit_graph_fanout(&object_ids)?),
            (*b"OIDL", write_commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        write_commit_graph_chunks(format, 0, &chunks)
    }

    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 8 + 12 + hash_len {
            return Err(GitError::InvalidFormat(
                "commit-graph file too short".into(),
            ));
        }
        if &bytes[..4] != b"CGPH" {
            return Err(GitError::InvalidFormat(
                "missing commit-graph signature".into(),
            ));
        }
        let version = bytes[4];
        if version != 1 {
            return Err(GitError::Unsupported(format!(
                "commit-graph version {version}"
            )));
        }
        let hash_id = bytes[5];
        if u32::from(hash_id) != hash_function_id(format) {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph hash id {hash_id} does not match {}",
                format.name()
            )));
        }
        let chunk_count = bytes[6] as usize;
        let base_graph_count = bytes[7];
        let lookup_len = (chunk_count + 1)
            .checked_mul(12)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let data_start = 8usize
            .checked_add(lookup_len)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
        let checksum_offset = bytes.len() - hash_len;
        if data_start > checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated commit-graph chunk lookup".into(),
            ));
        }

        let actual_checksum = sley_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }

        let mut lookup = Vec::with_capacity(chunk_count + 1);
        let mut offset = 8usize;
        for _ in 0..=chunk_count {
            let id = [
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ];
            let chunk_offset = u64_be(&bytes[offset + 4..offset + 12]);
            lookup.push((id, chunk_offset));
            offset += 12;
        }
        let Some((terminator_id, terminator_offset)) = lookup.last().copied() else {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup is empty".into(),
            ));
        };
        if terminator_id != [0, 0, 0, 0] {
            return Err(GitError::InvalidFormat(
                "commit-graph chunk lookup missing terminator".into(),
            ));
        }
        if terminator_offset != checksum_offset as u64 {
            return Err(GitError::InvalidFormat(
                "commit-graph terminator does not point at checksum".into(),
            ));
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut previous_offset = data_start as u64;
        for pair in lookup.windows(2) {
            let (id, chunk_offset) = pair[0];
            let (_next_id, next_offset) = pair[1];
            if id == [0, 0, 0, 0] {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is zero before terminator".into(),
                ));
            }
            if chunks.iter().any(|chunk: &CommitGraphChunk| chunk.id == id) {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk id is duplicated".into(),
                ));
            }
            if chunk_offset < data_start as u64 || chunk_offset < previous_offset {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk offsets are not monotonic".into(),
                ));
            }
            if next_offset < chunk_offset || next_offset > checksum_offset as u64 {
                return Err(GitError::InvalidFormat(
                    "commit-graph chunk length is invalid".into(),
                ));
            }
            chunks.push(CommitGraphChunk {
                id,
                offset: chunk_offset,
                len: next_offset - chunk_offset,
            });
            previous_offset = chunk_offset;
        }

        let (fanout, commit_count) = parse_commit_graph_fanout(bytes, &chunks)?;
        let oids = parse_commit_graph_oids(bytes, &chunks, format, commit_count, &fanout)?;
        let mut commits = parse_commit_graph_commit_data(bytes, &chunks, format, oids)?;
        apply_commit_graph_generation_data(bytes, &chunks, &mut commits)?;
        let bloom_filters = parse_commit_graph_bloom_filters(bytes, &chunks, commits.len())?;
        let base_graphs =
            parse_commit_graph_base_graphs(bytes, &chunks, format, base_graph_count as usize)?;

        Ok(Self {
            version,
            format,
            base_graph_count,
            fanout,
            commits,
            chunks,
            base_graphs,
            bloom_filters,
            checksum,
        })
    }

    pub fn find(&self, oid: &ObjectId) -> Option<&CommitGraphEntry> {
        self.commits
            .binary_search_by(|entry| entry.oid.as_bytes().cmp(oid.as_bytes()))
            .ok()
            .map(|idx| &self.commits[idx])
    }
}

fn validate_commit_graph_write_entries(
    format: ObjectFormat,
    entries: &[CommitGraphWriteEntry],
) -> Result<()> {
    let mut previous_oid: Option<&ObjectId> = None;
    for entry in entries {
        if entry.oid.format() != format
            || entry.tree.format() != format
            || entry.parents.iter().any(|parent| parent.format() != format)
        {
            return Err(GitError::InvalidObjectId(
                "commit-graph entry format does not match graph format".into(),
            ));
        }
        if let Some(previous) = previous_oid
            && previous.as_bytes() == entry.oid.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "commit-graph contains duplicate object ids".into(),
            ));
        }
        if entry.generation >= (1 << 30) {
            return Err(GitError::InvalidFormat(
                "commit-graph generation is too large".into(),
            ));
        }
        previous_oid = Some(&entry.oid);
    }
    Ok(())
}

fn write_commit_graph_fanout(object_ids: &[ObjectId]) -> Result<Vec<u8>> {
    let mut counts = [0u32; 256];
    for oid in object_ids {
        let first = oid.as_bytes()[0] as usize;
        counts[first] = counts[first]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
    }
    let mut running = 0u32;
    let mut out = Vec::with_capacity(256 * 4);
    for count in counts {
        running = running
            .checked_add(count)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        out.extend_from_slice(&running.to_be_bytes());
    }
    Ok(out)
}

fn write_commit_graph_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
    let mut out = Vec::new();
    for oid in object_ids {
        out.extend_from_slice(oid.as_bytes());
    }
    out
}

fn write_commit_graph_commit_data(entries: &[CommitGraphWriteEntry]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut cdat = Vec::new();
    let mut edge = Vec::new();
    for entry in entries {
        cdat.extend_from_slice(entry.tree.as_bytes());
        let parent_positions = entry
            .parents
            .iter()
            .map(|parent| {
                entries
                    .binary_search_by(|entry| entry.oid.as_bytes().cmp(parent.as_bytes()))
                    .map(|idx| idx as u32)
                    .map_err(|_| {
                        GitError::InvalidFormat(format!(
                            "commit-graph parent {parent} is missing from graph"
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let first_parent = parent_positions
            .first()
            .copied()
            .unwrap_or(COMMIT_GRAPH_PARENT_NONE);
        let second_parent = match parent_positions.len() {
            0 | 1 => COMMIT_GRAPH_PARENT_NONE,
            2 => parent_positions[1],
            _ => {
                let edge_start = u32::try_from(edge.len() / 4).map_err(|_| {
                    GitError::InvalidFormat("commit-graph EDGE chunk overflow".into())
                })?;
                for (idx, parent) in parent_positions[1..].iter().enumerate() {
                    let mut value = *parent;
                    if idx == parent_positions.len() - 2 {
                        value |= COMMIT_GRAPH_EXTRA_EDGE;
                    }
                    edge.extend_from_slice(&value.to_be_bytes());
                }
                COMMIT_GRAPH_EXTRA_EDGE | edge_start
            }
        };
        cdat.extend_from_slice(&first_parent.to_be_bytes());
        cdat.extend_from_slice(&second_parent.to_be_bytes());
        let generation_and_time_high =
            (entry.generation << 2) | (((entry.commit_time >> 32) as u32) & 0x3);
        cdat.extend_from_slice(&generation_and_time_high.to_be_bytes());
        cdat.extend_from_slice(&(entry.commit_time as u32).to_be_bytes());
    }
    Ok((cdat, edge))
}

fn write_commit_graph_chunks(
    format: ObjectFormat,
    base_graph_count: u8,
    chunks: &[([u8; 4], Vec<u8>)],
) -> Result<Vec<u8>> {
    if chunks.len() > u8::MAX as usize {
        return Err(GitError::InvalidFormat(
            "too many commit-graph chunks".into(),
        ));
    }
    let lookup_len = (chunks.len() + 1)
        .checked_mul(12)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?;
    let mut out = Vec::new();
    out.extend_from_slice(b"CGPH");
    out.push(1);
    out.push(hash_function_id(format) as u8);
    out.push(chunks.len() as u8);
    out.push(base_graph_count);
    let mut chunk_offset = (8usize)
        .checked_add(lookup_len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph lookup overflow".into()))?
        as u64;
    for (id, data) in chunks {
        out.extend_from_slice(id);
        out.extend_from_slice(&chunk_offset.to_be_bytes());
        chunk_offset = chunk_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph size overflow".into()))?;
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

const COMMIT_GRAPH_PARENT_NONE: u32 = 0x7000_0000;
const COMMIT_GRAPH_EXTRA_EDGE: u32 = 0x8000_0000;
const COMMIT_GRAPH_EXTRA_EDGE_MASK: u32 = 0x7fff_ffff;

fn parse_commit_graph_fanout(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
) -> Result<([u32; 256], usize)> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"OIDF", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDF chunk".into()))?;
    if data.len() != 256 * 4 {
        return Err(GitError::InvalidFormat(
            "commit-graph OIDF chunk has invalid length".into(),
        ));
    }
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = u32_be(&data[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDF fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255] as usize))
}

fn parse_commit_graph_oids(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    commit_count: usize,
    fanout: &[u32; 256],
) -> Result<Vec<ObjectId>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"OIDL", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing OIDL chunk".into()))?;
    let expected_len = commit_count
        .checked_mul(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("commit-graph OIDL chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph OIDL chunk has invalid length".into(),
        ));
    }

    let mut oids = Vec::with_capacity(commit_count);
    let mut counts = [0u32; 256];
    let mut previous_oid: Option<ObjectId> = None;
    for idx in 0..commit_count {
        let start = idx * format.raw_len();
        let oid = ObjectId::from_raw(format, &data[start..start + format.raw_len()])?;
        if let Some(previous) = &previous_oid
            && previous.as_bytes() >= oid.as_bytes()
        {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDL object ids are not strictly sorted".into(),
            ));
        }
        counts[oid.as_bytes()[0] as usize] = counts[oid.as_bytes()[0] as usize]
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        previous_oid = Some(oid);
        oids.push(oid);
    }

    let mut running = 0u32;
    for (idx, count) in counts.iter().enumerate() {
        running = running
            .checked_add(*count)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph fanout overflow".into()))?;
        if fanout[idx] != running {
            return Err(GitError::InvalidFormat(
                "commit-graph OIDF fanout does not match OIDL".into(),
            ));
        }
    }
    Ok(oids)
}

fn parse_commit_graph_commit_data(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    oids: Vec<ObjectId>,
) -> Result<Vec<CommitGraphEntry>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"CDAT", true)?
        .ok_or_else(|| GitError::InvalidFormat("commit-graph missing CDAT chunk".into()))?;
    let entry_len = format
        .raw_len()
        .checked_add(16)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT entry overflow".into()))?;
    let expected_len = oids
        .len()
        .checked_mul(entry_len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph CDAT chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph CDAT chunk has invalid length".into(),
        ));
    }
    let extra_edges = commit_graph_chunk_data(bytes, chunks, *b"EDGE", false)?;
    if let Some(extra_edges) = extra_edges
        && extra_edges.len() % 4 != 0
    {
        return Err(GitError::InvalidFormat(
            "commit-graph EDGE chunk has invalid length".into(),
        ));
    }

    let commit_count = oids.len();
    let mut entries = Vec::with_capacity(commit_count);
    for (idx, oid) in oids.into_iter().enumerate() {
        let start = idx * entry_len;
        let tree = ObjectId::from_raw(format, &data[start..start + format.raw_len()])?;
        let parent_one = u32_be(&data[start + format.raw_len()..start + format.raw_len() + 4]);
        let parent_two = u32_be(&data[start + format.raw_len() + 4..start + format.raw_len() + 8]);
        let generation_and_time_high =
            u32_be(&data[start + format.raw_len() + 8..start + format.raw_len() + 12]);
        let time_low = u32_be(&data[start + format.raw_len() + 12..start + entry_len]);
        let generation = generation_and_time_high >> 2;
        let commit_time = (u64::from(generation_and_time_high & 0x3) << 32) | u64::from(time_low);
        let parents = commit_graph_parents(parent_one, parent_two, extra_edges, commit_count)?;
        entries.push(CommitGraphEntry {
            oid,
            tree,
            parents,
            generation,
            commit_time,
            corrected_commit_date_offset: None,
        });
    }
    Ok(entries)
}

fn apply_commit_graph_generation_data(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    commits: &mut [CommitGraphEntry],
) -> Result<()> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"GDA2", false)?;
    let overflow = commit_graph_chunk_data(bytes, chunks, *b"GDO2", false)?;
    let Some(data) = data else {
        if overflow.is_some() {
            return Err(GitError::InvalidFormat(
                "commit-graph GDO2 chunk exists without GDA2 chunk".into(),
            ));
        }
        return Ok(());
    };
    let expected_len = commits
        .len()
        .checked_mul(4)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph GDA2 chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph GDA2 chunk has invalid length".into(),
        ));
    }
    if let Some(overflow) = overflow
        && overflow.len() % 8 != 0
    {
        return Err(GitError::InvalidFormat(
            "commit-graph GDO2 chunk has invalid length".into(),
        ));
    }

    let mut used_overflow = false;
    for (idx, commit) in commits.iter_mut().enumerate() {
        let start = idx * 4;
        let raw = u32_be(&data[start..start + 4]);
        let offset = if raw & 0x8000_0000 == 0 {
            u64::from(raw)
        } else {
            used_overflow = true;
            let Some(overflow) = overflow else {
                return Err(GitError::InvalidFormat(
                    "commit-graph GDA2 overflow entry missing GDO2 chunk".into(),
                ));
            };
            let overflow_idx = (raw & 0x7fff_ffff) as usize;
            let overflow_start = overflow_idx.checked_mul(8).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph GDO2 index overflow".into())
            })?;
            let overflow_end = overflow_start.checked_add(8).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph GDO2 index overflow".into())
            })?;
            if overflow_end > overflow.len() {
                return Err(GitError::InvalidFormat(
                    "commit-graph GDA2 overflow points past GDO2 chunk".into(),
                ));
            }
            u64_be(&overflow[overflow_start..overflow_end])
        };
        commit.corrected_commit_date_offset = Some(offset);
    }
    if overflow.is_some() && !used_overflow {
        return Err(GitError::InvalidFormat(
            "commit-graph GDO2 chunk is unused by GDA2".into(),
        ));
    }
    Ok(())
}

fn parse_commit_graph_bloom_filters(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    commit_count: usize,
) -> Result<Option<CommitGraphBloomFilters>> {
    let index = commit_graph_chunk_data(bytes, chunks, *b"BIDX", false)?;
    let data = commit_graph_chunk_data(bytes, chunks, *b"BDAT", false)?;
    let Some(data) = data else {
        return Ok(None);
    };
    let Some(index) = index else {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT chunk exists without BIDX chunk".into(),
        ));
    };
    let expected_index_len = commit_count
        .checked_mul(4)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph BIDX chunk overflow".into()))?;
    if index.len() != expected_index_len {
        return Err(GitError::InvalidFormat(
            "commit-graph BIDX chunk has invalid length".into(),
        ));
    }
    if data.len() < 12 {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT chunk has invalid length".into(),
        ));
    }
    let hash_version = u32_be(&data[0..4]);
    let hash_count = u32_be(&data[4..8]);
    let bits_per_entry = u32_be(&data[8..12]);
    let payload = &data[12..];

    let mut filters = Vec::with_capacity(commit_count);
    let mut previous = 0usize;
    for idx in 0..commit_count {
        let start = idx * 4;
        let cumulative = u32_be(&index[start..start + 4]) as usize;
        if cumulative < previous {
            return Err(GitError::InvalidFormat(
                "commit-graph BIDX offsets are not monotonic".into(),
            ));
        }
        if cumulative > payload.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph BIDX offset points past BDAT payload".into(),
            ));
        }
        filters.push(payload[previous..cumulative].to_vec());
        previous = cumulative;
    }
    if previous != payload.len() {
        return Err(GitError::InvalidFormat(
            "commit-graph BDAT payload has trailing bytes".into(),
        ));
    }

    Ok(Some(CommitGraphBloomFilters {
        hash_version,
        hash_count,
        bits_per_entry,
        filters,
    }))
}

fn commit_graph_parents(
    parent_one: u32,
    parent_two: u32,
    extra_edges: Option<&[u8]>,
    commit_count: usize,
) -> Result<Vec<u32>> {
    let mut parents = Vec::new();
    if parent_one != COMMIT_GRAPH_PARENT_NONE {
        validate_commit_graph_parent_position(parent_one, commit_count)?;
        parents.push(parent_one);
    }
    if parent_two == COMMIT_GRAPH_PARENT_NONE {
        return Ok(parents);
    }
    if parent_two & COMMIT_GRAPH_EXTRA_EDGE == 0 {
        validate_commit_graph_parent_position(parent_two, commit_count)?;
        parents.push(parent_two);
        return Ok(parents);
    }

    let Some(extra_edges) = extra_edges else {
        return Err(GitError::InvalidFormat(
            "commit-graph octopus edge missing EDGE chunk".into(),
        ));
    };
    let mut edge_idx = (parent_two & COMMIT_GRAPH_EXTRA_EDGE_MASK) as usize;
    loop {
        let start = edge_idx
            .checked_mul(4)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
        let end = start
            .checked_add(4)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
        if end > extra_edges.len() {
            return Err(GitError::InvalidFormat(
                "commit-graph EDGE entry points past chunk".into(),
            ));
        }
        let edge = u32_be(&extra_edges[start..end]);
        let parent = edge & COMMIT_GRAPH_EXTRA_EDGE_MASK;
        validate_commit_graph_parent_position(parent, commit_count)?;
        parents.push(parent);
        if edge & COMMIT_GRAPH_EXTRA_EDGE != 0 {
            return Ok(parents);
        }
        edge_idx = edge_idx
            .checked_add(1)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph EDGE index overflow".into()))?;
    }
}

fn validate_commit_graph_parent_position(parent: u32, commit_count: usize) -> Result<()> {
    if parent as usize >= commit_count {
        return Err(GitError::InvalidFormat(
            "commit-graph parent points past commit table".into(),
        ));
    }
    Ok(())
}

fn parse_commit_graph_base_graphs(
    bytes: &[u8],
    chunks: &[CommitGraphChunk],
    format: ObjectFormat,
    base_graph_count: usize,
) -> Result<Vec<ObjectId>> {
    let data = commit_graph_chunk_data(bytes, chunks, *b"BASE", base_graph_count != 0)?;
    let Some(data) = data else {
        return Ok(Vec::new());
    };
    let expected_len = base_graph_count
        .checked_mul(format.raw_len())
        .ok_or_else(|| GitError::InvalidFormat("commit-graph BASE chunk overflow".into()))?;
    if data.len() != expected_len {
        return Err(GitError::InvalidFormat(
            "commit-graph BASE chunk has invalid length".into(),
        ));
    }
    let mut base_graphs = Vec::with_capacity(base_graph_count);
    for idx in 0..base_graph_count {
        let start = idx * format.raw_len();
        base_graphs.push(ObjectId::from_raw(
            format,
            &data[start..start + format.raw_len()],
        )?);
    }
    Ok(base_graphs)
}

fn commit_graph_chunk_data<'a>(
    bytes: &'a [u8],
    chunks: &[CommitGraphChunk],
    id: [u8; 4],
    required: bool,
) -> Result<Option<&'a [u8]>> {
    let Some(chunk) = chunks.iter().find(|chunk| chunk.id == id) else {
        if required {
            return Err(GitError::InvalidFormat(format!(
                "commit-graph missing {} chunk",
                std::str::from_utf8(&id).unwrap_or("required")
            )));
        }
        return Ok(None);
    };
    let start = usize::try_from(chunk.offset)
        .map_err(|_| GitError::InvalidFormat("commit-graph chunk offset overflow".into()))?;
    let len = usize::try_from(chunk.len)
        .map_err(|_| GitError::InvalidFormat("commit-graph chunk length overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("commit-graph chunk range overflow".into()))?;
    let Some(data) = bytes.get(start..end) else {
        return Err(GitError::InvalidFormat(
            "commit-graph chunk extends past file".into(),
        ));
    };
    Ok(Some(data))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub version: u8,
    pub format: ObjectFormat,
    pub capabilities: Vec<BundleCapability>,
    pub prerequisites: Vec<BundlePrerequisite>,
    pub references: Vec<BundleReference>,
    pub pack: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleCapability {
    pub key: String,
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePrerequisite {
    pub oid: ObjectId,
    pub comment: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleReference {
    pub oid: ObjectId,
    pub name: String,
}

impl Bundle {
    pub fn parse_standalone(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn parse(bytes: &[u8], default_format: ObjectFormat) -> Result<Self> {
        let (signature, mut offset) = next_lf_line(bytes, 0)
            .ok_or_else(|| GitError::InvalidFormat("bundle missing signature".into()))?;
        let version = match signature {
            b"# v2 git bundle" => 2,
            b"# v3 git bundle" => 3,
            _ => {
                return Err(GitError::InvalidFormat("missing bundle signature".into()));
            }
        };

        let mut format = default_format;
        let mut capabilities = Vec::new();
        let mut prerequisites = Vec::new();
        let mut references = Vec::new();
        let mut seen_non_capability = false;
        loop {
            let Some((line, next_offset)) = next_lf_line(bytes, offset) else {
                return Err(GitError::InvalidFormat(
                    "bundle header missing pack separator".into(),
                ));
            };
            offset = next_offset;
            if line.is_empty() {
                break;
            }
            match line[0] {
                b'@' => {
                    if version != 3 {
                        return Err(GitError::InvalidFormat(
                            "bundle v2 cannot contain capabilities".into(),
                        ));
                    }
                    if seen_non_capability {
                        return Err(GitError::InvalidFormat(
                            "bundle capability appears after prerequisites or references".into(),
                        ));
                    }
                    let capability = parse_bundle_capability(&line[1..])?;
                    if capability.key == "object-format" {
                        let Some(value) = &capability.value else {
                            return Err(GitError::InvalidFormat(
                                "bundle object-format capability is missing a value".into(),
                            ));
                        };
                        let text = std::str::from_utf8(value)
                            .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
                        format = text.parse()?;
                    }
                    capabilities.push(capability);
                }
                b'-' => {
                    seen_non_capability = true;
                    prerequisites.push(parse_bundle_prerequisite(&line[1..], format)?);
                }
                _ => {
                    seen_non_capability = true;
                    references.push(parse_bundle_reference(line, format)?);
                }
            }
        }

        Ok(Self {
            version,
            format,
            capabilities,
            prerequisites,
            references,
            pack: bytes[offset..].to_vec(),
        })
    }

    pub fn write(&self) -> Result<Vec<u8>> {
        if self.version != 2 && self.version != 3 {
            return Err(GitError::Unsupported(format!(
                "bundle version {}",
                self.version
            )));
        }
        if self.version == 2 && !self.capabilities.is_empty() {
            return Err(GitError::InvalidFormat(
                "bundle v2 cannot contain capabilities".into(),
            ));
        }
        let mut out = Vec::new();
        match self.version {
            2 => out.extend_from_slice(b"# v2 git bundle\n"),
            3 => out.extend_from_slice(b"# v3 git bundle\n"),
            _ => unreachable!(),
        }
        if self.version == 3 {
            let mut wrote_object_format = false;
            for capability in &self.capabilities {
                write_bundle_capability(&mut out, capability)?;
                if capability.key == "object-format" {
                    let Some(value) = &capability.value else {
                        return Err(GitError::InvalidFormat(
                            "bundle object-format capability is missing a value".into(),
                        ));
                    };
                    let text = std::str::from_utf8(value)
                        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
                    let format: ObjectFormat = text.parse()?;
                    if format != self.format {
                        return Err(GitError::InvalidFormat(format!(
                            "bundle object-format capability is {}, bundle uses {}",
                            format.name(),
                            self.format.name()
                        )));
                    }
                    wrote_object_format = true;
                }
            }
            if self.format != ObjectFormat::Sha1 && !wrote_object_format {
                out.extend_from_slice(b"@object-format=");
                out.extend_from_slice(self.format.name().as_bytes());
                out.push(b'\n');
            }
        }
        for prerequisite in &self.prerequisites {
            ensure_bundle_oid_format(&prerequisite.oid, self.format, "prerequisite")?;
            out.push(b'-');
            out.extend_from_slice(prerequisite.oid.to_hex().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&prerequisite.comment);
            out.push(b'\n');
        }
        for reference in &self.references {
            ensure_bundle_oid_format(&reference.oid, self.format, "reference")?;
            if reference.name.is_empty() || reference.name.as_bytes().contains(&b'\n') {
                return Err(GitError::InvalidFormat(
                    "bundle reference has invalid name".into(),
                ));
            }
            out.extend_from_slice(reference.oid.to_hex().as_bytes());
            out.push(b' ');
            out.extend_from_slice(reference.name.as_bytes());
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.pack);
        Ok(out)
    }
}

fn next_lf_line(bytes: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    let relative = bytes
        .get(offset..)?
        .iter()
        .position(|byte| *byte == b'\n')?;
    let end = offset + relative;
    Some((&bytes[offset..end], end + 1))
}

fn parse_bundle_capability(line: &[u8]) -> Result<BundleCapability> {
    let (key, value) = match line.iter().position(|byte| *byte == b'=') {
        Some(idx) => (&line[..idx], Some(line[idx + 1..].to_vec())),
        None => (line, None),
    };
    if key.is_empty()
        || !key
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    let key = std::str::from_utf8(key)
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    Ok(BundleCapability { key, value })
}

fn write_bundle_capability(out: &mut Vec<u8>, capability: &BundleCapability) -> Result<()> {
    if capability.key.is_empty()
        || !capability
            .key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    if capability.key.as_bytes().contains(&b'\n') {
        return Err(GitError::InvalidFormat(
            "bundle capability has invalid key".into(),
        ));
    }
    out.push(b'@');
    out.extend_from_slice(capability.key.as_bytes());
    if let Some(value) = &capability.value {
        if value.contains(&b'\n') {
            return Err(GitError::InvalidFormat(
                "bundle capability has invalid value".into(),
            ));
        }
        out.push(b'=');
        out.extend_from_slice(value);
    }
    out.push(b'\n');
    Ok(())
}

fn ensure_bundle_oid_format(oid: &ObjectId, format: ObjectFormat, kind: &str) -> Result<()> {
    if oid.format() != format {
        return Err(GitError::InvalidObjectId(format!(
            "bundle {kind} {oid} uses {}, bundle uses {}",
            oid.format().name(),
            format.name()
        )));
    }
    Ok(())
}

fn parse_bundle_prerequisite(line: &[u8], format: ObjectFormat) -> Result<BundlePrerequisite> {
    let hex_len = format.hex_len();
    if line.len() < hex_len + 1 || line.get(hex_len).copied() != Some(b' ') {
        return Err(GitError::InvalidFormat(
            "bundle prerequisite line is malformed".into(),
        ));
    }
    let hex = std::str::from_utf8(&line[..hex_len])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    Ok(BundlePrerequisite {
        oid: ObjectId::from_hex(format, hex)?,
        comment: line[hex_len + 1..].to_vec(),
    })
}

fn parse_bundle_reference(line: &[u8], format: ObjectFormat) -> Result<BundleReference> {
    let hex_len = format.hex_len();
    if line.len() <= hex_len + 1 || line.get(hex_len).copied() != Some(b' ') {
        return Err(GitError::InvalidFormat(
            "bundle reference line is malformed".into(),
        ));
    }
    let hex = std::str::from_utf8(&line[..hex_len])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    let name = std::str::from_utf8(&line[hex_len + 1..])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
    if name.is_empty() {
        return Err(GitError::InvalidFormat(
            "bundle reference has empty name".into(),
        ));
    }
    Ok(BundleReference {
        oid: ObjectId::from_hex(format, hex)?,
        name: name.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
    pub git_dir: PathBuf,
    pub object_format: ObjectFormat,
    pub bare: bool,
}

impl RepositoryLayout {
    pub fn init_at(
        path: impl AsRef<Path>,
        object_format: ObjectFormat,
        bare: bool,
    ) -> Result<Self> {
        Self::init_at_with_initial_branch(path, object_format, bare, "main")
    }

    pub fn init_at_with_initial_branch(
        path: impl AsRef<Path>,
        object_format: ObjectFormat,
        bare: bool,
        initial_branch: &str,
    ) -> Result<Self> {
        let root = path.as_ref();
        let git_dir = if bare {
            root.to_path_buf()
        } else {
            root.join(".git")
        };
        fs::create_dir_all(git_dir.join("objects/info"))?;
        fs::create_dir_all(git_dir.join("objects/pack"))?;
        fs::create_dir_all(git_dir.join("refs/heads"))?;
        fs::create_dir_all(git_dir.join("refs/tags"))?;
        let head_path = git_dir.join("HEAD");
        if !head_path.exists() {
            fs::write(head_path, format!("ref: refs/heads/{initial_branch}\n"))?;
        }
        let mut config = GitConfig {
            sections: vec![ConfigSection {
                name: "core".into(),
                subsection: None,
                entries: vec![
                    ConfigEntry {
                        key: "repositoryformatversion".into(),
                        value: Some(
                            if object_format == ObjectFormat::Sha1 {
                                "0"
                            } else {
                                "1"
                            }
                            .into(),
                        ),
                        comment: None,
                    },
                    ConfigEntry {
                        key: "filemode".into(),
                        value: Some("true".into()),
                        comment: None,
                    },
                    ConfigEntry {
                        key: "bare".into(),
                        value: Some(if bare { "true" } else { "false" }.into()),
                        comment: None,
                    },
                ],
            }],
        };
        if object_format == ObjectFormat::Sha256 {
            config.sections.push(ConfigSection {
                name: "extensions".into(),
                subsection: None,
                entries: vec![ConfigEntry {
                    key: "objectformat".into(),
                    value: Some("sha256".into()),
                    comment: None,
                }],
            });
        }
        fs::write(git_dir.join("config"), config.to_canonical_bytes())?;
        Ok(Self {
            git_dir,
            object_format,
            bare,
        })
    }
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
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
    use std::io::Write;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reftable_empty_table_round_trips() {
        let bytes = Reftable::write_ref_only(ObjectFormat::Sha1, 1, 1, &[])
            .expect("test operation should succeed");
        let table = Reftable::parse(&bytes).expect("test operation should succeed");

        assert_eq!(table.header.version, ReftableVersion::V1);
        assert_eq!(table.header.object_format, ObjectFormat::Sha1);
        assert_eq!(table.refs, Vec::new());
    }

    #[test]
    fn reftable_ref_only_table_round_trips_refs() {
        let head = oid("1111111111111111111111111111111111111111");
        let tag = oid("2222222222222222222222222222222222222222");
        let peeled = oid("3333333333333333333333333333333333333333");
        let refs = vec![
            ReftableRefRecord {
                name: "refs/tags/v1".into(),
                update_index: 7,
                value: ReftableRefValue::Peeled {
                    target: tag,
                    peeled: peeled.clone(),
                },
            },
            ReftableRefRecord {
                name: "HEAD".into(),
                update_index: 7,
                value: ReftableRefValue::Symbolic("refs/heads/main".into()),
            },
            ReftableRefRecord {
                name: "refs/heads/main".into(),
                update_index: 7,
                value: ReftableRefValue::Direct(head.clone()),
            },
        ];

        let bytes = Reftable::write_ref_only(ObjectFormat::Sha1, 7, 7, &refs)
            .expect("test operation should succeed");
        let table = Reftable::parse(&bytes).expect("test operation should succeed");

        assert_eq!(table.header.min_update_index, 7);
        assert_eq!(table.header.max_update_index, 7);
        assert_eq!(
            table.refs,
            vec![
                ReftableRefRecord {
                    name: "HEAD".into(),
                    update_index: 7,
                    value: ReftableRefValue::Symbolic("refs/heads/main".into()),
                },
                ReftableRefRecord {
                    name: "refs/heads/main".into(),
                    update_index: 7,
                    value: ReftableRefValue::Direct(head),
                },
                ReftableRefRecord {
                    name: "refs/tags/v1".into(),
                    update_index: 7,
                    value: ReftableRefValue::Peeled {
                        target: tag,
                        peeled,
                    },
                },
            ]
        );
    }

    #[test]
    fn reftable_sha256_uses_version_2_hash_id() {
        let oid = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let refs = vec![ReftableRefRecord {
            name: "refs/heads/main".into(),
            update_index: 3,
            value: ReftableRefValue::Direct(oid),
        }];

        let bytes = Reftable::write_ref_only(ObjectFormat::Sha256, 3, 3, &refs)
            .expect("test operation should succeed");
        let table = Reftable::parse(&bytes).expect("test operation should succeed");

        assert_eq!(table.header.version, ReftableVersion::V2);
        assert_eq!(table.header.object_format, ObjectFormat::Sha256);
        assert_eq!(table.refs[0].value, ReftableRefValue::Direct(oid));
    }

    #[test]
    fn upstream_git_reads_rust_written_minimal_reftable() {
        let root = unique_temp_dir("reftable-upstream");
        fs::create_dir_all(&root).expect("create temp repo");
        {
            run_success("git", &root, &["init", "-q"]);
            let oid = run_success_with_stdin(
                "git",
                &root,
                &["hash-object", "-w", "--stdin"],
                b"payload\n",
            );
            let oid = String::from_utf8(oid).expect("oid is utf8");
            let oid = ObjectId::from_hex(ObjectFormat::Sha1, oid.trim())
                .expect("test operation should succeed");
            let git_dir = root.join(".git");
            fs::write(
                git_dir.join("config"),
                b"[core]\n\trepositoryformatversion = 1\n[extensions]\n\trefStorage = reftable\n",
            )
            .expect("write config");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/.invalid\n").expect("write HEAD");
            let reftable_dir = git_dir.join("reftable");
            fs::create_dir_all(&reftable_dir).expect("create reftable dir");
            let table_name = "000000000001-000000000001-rust.ref";
            let table = Reftable::write_ref_only(
                ObjectFormat::Sha1,
                1,
                1,
                &[ReftableRefRecord {
                    name: "refs/heads/main".into(),
                    update_index: 1,
                    value: ReftableRefValue::Direct(oid),
                }],
            )
            .expect("test operation should succeed");
            fs::write(reftable_dir.join(table_name), table).expect("write reftable");
            fs::write(reftable_dir.join("tables.list"), format!("{table_name}\n"))
                .expect("write tables.list");

            let output = run_success("git", &root, &["show-ref"]);
            assert_eq!(
                String::from_utf8(output).expect("show-ref output is utf8"),
                format!("{oid} refs/heads/main\n")
            );
        };
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_commit_graph_core_chunks() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree,
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree,
                vec![0],
                2,
                2,
            ),
            (
                oid("3333333333333333333333333333333333333333"),
                tree,
                vec![1],
                3,
                3,
            ),
            (
                oid("4444444444444444444444444444444444444444"),
                tree,
                vec![0, 1, 2],
                4,
                0x1_0000_0001,
            ),
        ];
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &commit_graph_chunks(&commits));

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.base_graph_count, 0);
        assert_eq!(parsed.commits.len(), 4);
        let merge = parsed
            .find(&commits[3].0)
            .expect("test operation should succeed");
        assert_eq!(merge.parents, vec![0, 1, 2]);
        assert_eq!(merge.generation, 4);
        assert_eq!(merge.commit_time, 0x1_0000_0001);
        assert_eq!(merge.corrected_commit_date_offset, None);
        assert!(parsed.base_graphs.is_empty());
        assert_eq!(parsed.bloom_filters, None);
        assert_eq!(parsed.chunks[0].id, *b"OIDF");
    }

    #[test]
    fn writes_commit_graph_core_chunks_that_round_trip() {
        let base = oid("1111111111111111111111111111111111111111");
        let main = oid("2222222222222222222222222222222222222222");
        let side = oid("3333333333333333333333333333333333333333");
        let merge = oid("4444444444444444444444444444444444444444");
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let bytes = CommitGraph::write(
            ObjectFormat::Sha1,
            &[
                CommitGraphWriteEntry {
                    oid: merge.clone(),
                    tree,
                    parents: vec![main.clone(), side.clone()],
                    generation: 3,
                    commit_time: 30,
                },
                CommitGraphWriteEntry {
                    oid: base,
                    tree,
                    parents: Vec::new(),
                    generation: 1,
                    commit_time: 10,
                },
                CommitGraphWriteEntry {
                    oid: main.clone(),
                    tree,
                    parents: vec![base],
                    generation: 2,
                    commit_time: 20,
                },
                CommitGraphWriteEntry {
                    oid: side.clone(),
                    tree,
                    parents: vec![base],
                    generation: 2,
                    commit_time: 21,
                },
            ],
        )
        .expect("test operation should succeed");

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.commits.len(), 4);
        assert_eq!(
            parsed
                .find(&base)
                .expect("test operation should succeed")
                .parents,
            Vec::<u32>::new()
        );
        assert_eq!(
            parsed
                .find(&main)
                .expect("test operation should succeed")
                .parents,
            vec![0]
        );
        assert_eq!(
            parsed
                .find(&side)
                .expect("test operation should succeed")
                .parents,
            vec![0]
        );
        assert_eq!(
            parsed
                .find(&merge)
                .expect("test operation should succeed")
                .parents,
            vec![1, 2]
        );
        assert_eq!(
            parsed
                .find(&merge)
                .expect("test operation should succeed")
                .generation,
            3
        );
        assert_eq!(
            parsed
                .find(&merge)
                .expect("test operation should succeed")
                .commit_time,
            30
        );
    }

    #[test]
    fn parses_commit_graph_bloom_filters() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree,
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree,
                vec![0],
                2,
                2,
            ),
        ];
        let mut chunks = commit_graph_chunks(&commits);
        chunks.push((*b"BIDX", commit_graph_bidx(&[2, 3])));
        chunks.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa, 0xbb, 0xcc])));
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &chunks);

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(
            parsed.bloom_filters,
            Some(CommitGraphBloomFilters {
                hash_version: 2,
                hash_count: 7,
                bits_per_entry: 10,
                filters: vec![vec![0xaa, 0xbb], vec![0xcc]],
            })
        );
    }

    #[test]
    fn parses_commit_graph_generation_data() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree,
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree,
                vec![0],
                2,
                2,
            ),
        ];
        let mut chunks = commit_graph_chunks(&commits);
        chunks.push((*b"GDA2", commit_graph_gda2(&[7, 0x8000_0000])));
        chunks.push((*b"GDO2", commit_graph_gdo2(&[0x1_0000_0007])));
        let bytes = commit_graph(ObjectFormat::Sha1, 0, &chunks);

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.commits[0].corrected_commit_date_offset, Some(7));
        assert_eq!(
            parsed.commits[1].corrected_commit_date_offset,
            Some(0x1_0000_0007)
        );
    }

    #[test]
    fn parses_commit_graph_base_graph_hashes() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );
        let base_graph = oid("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut chunks = commit_graph_chunks(&[commit]);
        chunks.push((*b"BASE", base_graph.as_bytes().to_vec()));
        let bytes = commit_graph(ObjectFormat::Sha1, 1, &chunks);

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.base_graph_count, 1);
        assert_eq!(parsed.base_graphs, vec![base_graph]);
    }

    #[test]
    fn rejects_bad_commit_graph_shape() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );
        let chunks = commit_graph_chunks(std::slice::from_ref(&commit));
        let mut bad_checksum = commit_graph(ObjectFormat::Sha1, 0, &chunks);
        let last = bad_checksum.len() - 1;
        bad_checksum[last] ^= 1;
        assert!(CommitGraph::parse(&bad_checksum, ObjectFormat::Sha1).is_err());

        let missing_cdat = commit_graph(
            ObjectFormat::Sha1,
            0,
            &chunks
                .iter()
                .filter(|(id, _data)| id != b"CDAT")
                .cloned()
                .collect::<Vec<_>>(),
        );
        assert!(CommitGraph::parse(&missing_cdat, ObjectFormat::Sha1).is_err());

        let bad_fanout = commit_graph(
            ObjectFormat::Sha1,
            0,
            &[
                (*b"OIDF", vec![0; 256 * 4]),
                (*b"OIDL", commit.0.as_bytes().to_vec()),
                (*b"CDAT", commit_graph_cdat(std::slice::from_ref(&commit)).0),
            ],
        );
        assert!(CommitGraph::parse(&bad_fanout, ObjectFormat::Sha1).is_err());

        let octopus = (
            oid("2222222222222222222222222222222222222222"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            vec![0, 0, 0],
            2,
            2,
        );
        let missing_edge = commit_graph(
            ObjectFormat::Sha1,
            0,
            &commit_graph_chunks(&[commit.clone(), octopus])
                .into_iter()
                .filter(|(id, _data)| id != b"EDGE")
                .collect::<Vec<_>>(),
        );
        assert!(CommitGraph::parse(&missing_edge, ObjectFormat::Sha1).is_err());

        let mut bad_base = commit_graph_chunks(&[commit]);
        bad_base.push((*b"BASE", vec![0]));
        let bad_base = commit_graph(ObjectFormat::Sha1, 1, &bad_base);
        assert!(CommitGraph::parse(&bad_base, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_commit_graph_generation_data() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );

        let mut short_gda2 = commit_graph_chunks(std::slice::from_ref(&commit));
        short_gda2.push((*b"GDA2", vec![0]));
        let short_gda2 = commit_graph(ObjectFormat::Sha1, 0, &short_gda2);
        assert!(CommitGraph::parse(&short_gda2, ObjectFormat::Sha1).is_err());

        let mut missing_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        missing_gdo2.push((*b"GDA2", commit_graph_gda2(&[0x8000_0000])));
        let missing_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &missing_gdo2);
        assert!(CommitGraph::parse(&missing_gdo2, ObjectFormat::Sha1).is_err());

        let mut bad_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        bad_gdo2.push((*b"GDA2", commit_graph_gda2(&[0x8000_0000])));
        bad_gdo2.push((*b"GDO2", vec![0]));
        let bad_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &bad_gdo2);
        assert!(CommitGraph::parse(&bad_gdo2, ObjectFormat::Sha1).is_err());

        let mut unused_gdo2 = commit_graph_chunks(std::slice::from_ref(&commit));
        unused_gdo2.push((*b"GDA2", commit_graph_gda2(&[1])));
        unused_gdo2.push((*b"GDO2", commit_graph_gdo2(&[2])));
        let unused_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &unused_gdo2);
        assert!(CommitGraph::parse(&unused_gdo2, ObjectFormat::Sha1).is_err());

        let mut orphan_gdo2 = commit_graph_chunks(&[commit]);
        orphan_gdo2.push((*b"GDO2", commit_graph_gdo2(&[2])));
        let orphan_gdo2 = commit_graph(ObjectFormat::Sha1, 0, &orphan_gdo2);
        assert!(CommitGraph::parse(&orphan_gdo2, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn rejects_bad_commit_graph_bloom_filters() {
        let commit = (
            oid("1111111111111111111111111111111111111111"),
            oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Vec::new(),
            1,
            1,
        );

        let mut bidx_without_bdat = commit_graph_chunks(std::slice::from_ref(&commit));
        bidx_without_bdat.push((*b"BIDX", commit_graph_bidx(&[1])));
        let bidx_without_bdat = commit_graph(ObjectFormat::Sha1, 0, &bidx_without_bdat);
        assert_eq!(
            CommitGraph::parse(&bidx_without_bdat, ObjectFormat::Sha1)
                .expect("test operation should succeed")
                .bloom_filters,
            None
        );

        let mut bdat_without_bidx = commit_graph_chunks(std::slice::from_ref(&commit));
        bdat_without_bidx.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let bdat_without_bidx = commit_graph(ObjectFormat::Sha1, 0, &bdat_without_bidx);
        assert!(CommitGraph::parse(&bdat_without_bidx, ObjectFormat::Sha1).is_err());

        let mut short_bidx = commit_graph_chunks(std::slice::from_ref(&commit));
        short_bidx.push((*b"BIDX", vec![0]));
        short_bidx.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let short_bidx = commit_graph(ObjectFormat::Sha1, 0, &short_bidx);
        assert!(CommitGraph::parse(&short_bidx, ObjectFormat::Sha1).is_err());

        let mut short_bdat = commit_graph_chunks(std::slice::from_ref(&commit));
        short_bdat.push((*b"BIDX", commit_graph_bidx(&[0])));
        short_bdat.push((*b"BDAT", vec![0]));
        let short_bdat = commit_graph(ObjectFormat::Sha1, 0, &short_bdat);
        assert!(CommitGraph::parse(&short_bdat, ObjectFormat::Sha1).is_err());

        let mut bidx_past_payload = commit_graph_chunks(std::slice::from_ref(&commit));
        bidx_past_payload.push((*b"BIDX", commit_graph_bidx(&[2])));
        bidx_past_payload.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa])));
        let bidx_past_payload = commit_graph(ObjectFormat::Sha1, 0, &bidx_past_payload);
        assert!(CommitGraph::parse(&bidx_past_payload, ObjectFormat::Sha1).is_err());

        let mut trailing_payload = commit_graph_chunks(&[commit]);
        trailing_payload.push((*b"BIDX", commit_graph_bidx(&[1])));
        trailing_payload.push((*b"BDAT", commit_graph_bdat(2, 7, 10, &[0xaa, 0xbb])));
        let trailing_payload = commit_graph(ObjectFormat::Sha1, 0, &trailing_payload);
        assert!(CommitGraph::parse(&trailing_payload, ObjectFormat::Sha1).is_err());
    }

    #[test]
    fn parses_bundle_v2_header_and_pack() {
        let prerequisite = oid("1111111111111111111111111111111111111111");
        let reference = oid("2222222222222222222222222222222222222222");
        let bytes = format!(
            "# v2 git bundle\n-{prerequisite} prerequisite comment\n{reference} refs/heads/main\n\n"
        )
        .into_bytes()
        .into_iter()
        .chain(b"PACKdata".iter().copied())
        .collect::<Vec<_>>();

        let parsed =
            Bundle::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert!(parsed.capabilities.is_empty());
        assert_eq!(
            parsed.prerequisites,
            vec![BundlePrerequisite {
                oid: prerequisite,
                comment: b"prerequisite comment".to_vec(),
            }]
        );
        assert_eq!(
            parsed.references,
            vec![BundleReference {
                oid: reference,
                name: "refs/heads/main".into(),
            }]
        );
        assert_eq!(parsed.pack, b"PACKdata");
    }

    #[test]
    fn parses_bundle_v3_capabilities_and_sha256_ids() {
        let prerequisite = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .expect("test operation should succeed");
        let reference = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "2222222222222222222222222222222222222222222222222222222222222222",
        )
        .expect("test operation should succeed");
        let bytes = format!(
            "# v3 git bundle\n@object-format=sha256\n@filter=blob:none\n-{prerequisite} base\n{reference} refs/heads/main\n\n"
        )
        .into_bytes();

        let parsed =
            Bundle::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.version, 3);
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(
            parsed.capabilities,
            vec![
                BundleCapability {
                    key: "object-format".into(),
                    value: Some(b"sha256".to_vec()),
                },
                BundleCapability {
                    key: "filter".into(),
                    value: Some(b"blob:none".to_vec()),
                },
            ]
        );
        assert_eq!(parsed.prerequisites[0].oid, prerequisite);
        assert_eq!(parsed.references[0].oid, reference);
    }

    #[test]
    fn standalone_bundle_parse_uses_sha1_default_and_header_object_format_override() {
        let sha1 = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n")
            .expect("test operation should succeed");
        let sha256 = sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n")
            .expect("test operation should succeed");

        let sha1_bytes = format!("# v2 git bundle\n{sha1} refs/heads/main\n\nPACK").into_bytes();
        let sha1_bundle =
            Bundle::parse_standalone(&sha1_bytes).expect("test operation should succeed");
        assert_eq!(sha1_bundle.format, ObjectFormat::Sha1);
        assert_eq!(sha1_bundle.references[0].oid, sha1);

        let sha256_bytes =
            format!("# v3 git bundle\n@object-format=sha256\n{sha256} refs/heads/main\n\nPACK")
                .into_bytes();
        let sha256_bundle =
            Bundle::parse_standalone(&sha256_bytes).expect("test operation should succeed");
        assert_eq!(sha256_bundle.format, ObjectFormat::Sha256);
        assert_eq!(sha256_bundle.references[0].oid, sha256);
    }

    #[test]
    fn writes_bundle_v2_header_and_pack() {
        let prerequisite = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"base\n")
            .expect("test operation should succeed");
        let reference = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n")
            .expect("test operation should succeed");
        let bundle = Bundle {
            version: 2,
            format: ObjectFormat::Sha1,
            capabilities: Vec::new(),
            prerequisites: vec![BundlePrerequisite {
                oid: prerequisite.clone(),
                comment: b"base comment".to_vec(),
            }],
            references: vec![BundleReference {
                oid: reference.clone(),
                name: "refs/heads/main".into(),
            }],
            pack: b"PACKv2".to_vec(),
        };

        let bytes = bundle.write().expect("test operation should succeed");
        let expected = format!(
            "# v2 git bundle\n-{prerequisite} base comment\n{reference} refs/heads/main\n\n"
        )
        .into_bytes()
        .into_iter()
        .chain(b"PACKv2".iter().copied())
        .collect::<Vec<_>>();
        assert_eq!(bytes, expected);
        assert_eq!(
            Bundle::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed"),
            bundle
        );
    }

    #[test]
    fn writes_bundle_v3_sha256_object_format_capability() {
        let oid = sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n")
            .expect("test operation should succeed");
        let bundle = Bundle {
            version: 3,
            format: ObjectFormat::Sha256,
            capabilities: vec![BundleCapability {
                key: "filter".into(),
                value: Some(b"blob:none".to_vec()),
            }],
            prerequisites: Vec::new(),
            references: vec![BundleReference {
                oid,
                name: "refs/heads/main".into(),
            }],
            pack: b"PACKv3".to_vec(),
        };

        let bytes = bundle.write().expect("test operation should succeed");
        let text = String::from_utf8(bytes.clone()).expect("test operation should succeed");
        assert!(text.starts_with("# v3 git bundle\n@filter=blob:none\n@object-format=sha256\n"));
        let parsed =
            Bundle::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(parsed.references[0].oid, oid);
        assert_eq!(parsed.pack, b"PACKv3");
    }

    #[test]
    fn rejects_bad_bundle_write_inputs() {
        let sha1 = sley_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n")
            .expect("test operation should succeed");
        let sha256 = sley_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n")
            .expect("test operation should succeed");
        let mut bundle = Bundle {
            version: 2,
            format: ObjectFormat::Sha1,
            capabilities: vec![BundleCapability {
                key: "filter".into(),
                value: Some(b"blob:none".to_vec()),
            }],
            prerequisites: Vec::new(),
            references: Vec::new(),
            pack: Vec::new(),
        };
        assert!(bundle.write().is_err());

        bundle.version = 3;
        bundle.capabilities = vec![BundleCapability {
            key: "bad_key".into(),
            value: None,
        }];
        assert!(bundle.write().is_err());

        bundle.capabilities = Vec::new();
        bundle.references = vec![BundleReference {
            oid: sha256,
            name: "refs/heads/main".into(),
        }];
        assert!(bundle.write().is_err());

        bundle.references = vec![BundleReference {
            oid: sha1,
            name: "bad\nname".into(),
        }];
        assert!(bundle.write().is_err());
    }

    #[test]
    fn rejects_bad_bundle_headers() {
        assert!(Bundle::parse(b"# v4 git bundle\n\n", ObjectFormat::Sha1).is_err());
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n@filter=blob:none\n\n",
                ObjectFormat::Sha1
            )
            .is_err()
        );
        assert!(Bundle::parse(b"# v3 git bundle\n@bad_key=value\n\n", ObjectFormat::Sha1).is_err());
        assert!(
            Bundle::parse(
                b"# v3 git bundle\n1111111111111111111111111111111111111111 refs/heads/main\n@filter=blob:none\n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v3 git bundle\n@object-format=unknown\n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n1111111111111111111111111111111111111111 refs/heads/main",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
        assert!(
            Bundle::parse(
                b"# v2 git bundle\n1111111111111111111111111111111111111111 \n\n",
                ObjectFormat::Sha1,
            )
            .is_err()
        );
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).expect("test operation should succeed")
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sley-{name}-{}-{nanos}", std::process::id()))
    }

    fn run_success(program: &str, cwd: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run {program} {args:?}: {err}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn run_success_with_stdin(program: &str, cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
        let mut child = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to spawn {program} {args:?}: {err}"));
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(stdin)
            .expect("write stdin");
        let output = child
            .wait_with_output()
            .unwrap_or_else(|err| panic!("failed to wait for {program} {args:?}: {err}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn commit_graph(
        format: ObjectFormat,
        base_graph_count: u8,
        chunks: &[([u8; 4], Vec<u8>)],
    ) -> Vec<u8> {
        let lookup_len = (chunks.len() + 1) * 12;
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(hash_function_id(format) as u8);
        out.push(chunks.len() as u8);
        out.push(base_graph_count);
        let mut chunk_offset = (8 + lookup_len) as u64;
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
        let checksum =
            sley_core::digest_bytes(format, &out).expect("test operation should succeed");
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn commit_graph_chunks(
        entries: &[(ObjectId, ObjectId, Vec<u32>, u32, u64)],
    ) -> Vec<([u8; 4], Vec<u8>)> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let object_ids: Vec<ObjectId> = entries.iter().map(|entry| entry.0).collect();
        let (cdat, edge) = commit_graph_cdat(&entries);
        let mut chunks = vec![
            (*b"OIDF", commit_graph_fanout(&object_ids)),
            (*b"OIDL", commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        chunks
    }

    fn commit_graph_fanout(object_ids: &[ObjectId]) -> Vec<u8> {
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

    fn commit_graph_oid_lookup(object_ids: &[ObjectId]) -> Vec<u8> {
        let mut out = Vec::new();
        for oid in object_ids {
            out.extend_from_slice(oid.as_bytes());
        }
        out
    }

    fn commit_graph_cdat(
        entries: &[(ObjectId, ObjectId, Vec<u32>, u32, u64)],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut cdat = Vec::new();
        let mut edge = Vec::new();
        for (_oid, tree, parents, generation, commit_time) in entries {
            cdat.extend_from_slice(tree.as_bytes());
            let first_parent = parents.first().copied().unwrap_or(COMMIT_GRAPH_PARENT_NONE);
            let second_parent = match parents.len() {
                0 | 1 => COMMIT_GRAPH_PARENT_NONE,
                2 => parents[1],
                _ => {
                    let edge_start = (edge.len() / 4) as u32;
                    for (idx, parent) in parents[1..].iter().enumerate() {
                        let mut value = *parent;
                        if idx == parents.len() - 2 {
                            value |= COMMIT_GRAPH_EXTRA_EDGE;
                        }
                        edge.extend_from_slice(&value.to_be_bytes());
                    }
                    COMMIT_GRAPH_EXTRA_EDGE | edge_start
                }
            };
            cdat.extend_from_slice(&first_parent.to_be_bytes());
            cdat.extend_from_slice(&second_parent.to_be_bytes());
            let generation_and_time_high =
                (*generation << 2) | (((*commit_time >> 32) as u32) & 0x3);
            cdat.extend_from_slice(&generation_and_time_high.to_be_bytes());
            cdat.extend_from_slice(&(*commit_time as u32).to_be_bytes());
        }
        (cdat, edge)
    }

    fn commit_graph_gda2(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_gdo2(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_bidx(values: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        for value in values {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn commit_graph_bdat(
        hash_version: u32,
        hash_count: u32,
        bits_per_entry: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&hash_version.to_be_bytes());
        out.extend_from_slice(&hash_count.to_be_bytes());
        out.extend_from_slice(&bits_per_entry.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }
}
