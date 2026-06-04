use git_core::{GitError, ObjectFormat, ObjectId, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const REFTABLE_MAGIC: &[u8; 4] = b"REFT";
const REFTABLE_MAX_BLOCK_SIZE: u32 = 0x00ff_ffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

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

impl ObjectType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }
}

impl FromStr for ObjectType {
    type Err = GitError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "blob" => Ok(Self::Blob),
            "tree" => Ok(Self::Tree),
            "commit" => Ok(Self::Commit),
            "tag" => Ok(Self::Tag),
            other => Err(GitError::InvalidObject(format!(
                "unknown object type {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedObject {
    pub object_type: ObjectType,
    pub body: Vec<u8>,
}

impl EncodedObject {
    pub fn new(object_type: ObjectType, body: impl Into<Vec<u8>>) -> Self {
        Self {
            object_type,
            body: body.into(),
        }
    }

    pub fn framed_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 32);
        out.extend_from_slice(self.object_type.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.body.len().to_string().as_bytes());
        out.push(0);
        out.extend_from_slice(&self.body);
        out
    }

    pub fn object_id(&self, format: ObjectFormat) -> Result<ObjectId> {
        git_core::object_id_for_bytes(format, self.object_type.as_str(), &self.body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: Vec<u8>,
    pub oid: ObjectId,
}

impl Tree {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let mut offset = 0usize;
        let mut entries = Vec::new();
        while offset < bytes.len() {
            let mode_start = offset;
            while bytes.get(offset).copied() != Some(b' ') {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree mode".into()));
                }
            }
            let mode_text = std::str::from_utf8(&bytes[mode_start..offset])
                .map_err(|err| GitError::InvalidFormat(err.to_string()))?;
            let mode = u32::from_str_radix(mode_text, 8)
                .map_err(|_| GitError::InvalidFormat("invalid tree mode".into()))?;
            offset += 1;
            let name_start = offset;
            while bytes.get(offset).copied() != Some(0) {
                offset += 1;
                if offset >= bytes.len() {
                    return Err(GitError::InvalidFormat("unterminated tree path".into()));
                }
            }
            if offset == name_start {
                return Err(GitError::InvalidFormat("empty tree path".into()));
            }
            let name = bytes[name_start..offset].to_vec();
            offset += 1;
            let oid_end = offset
                .checked_add(format.raw_len())
                .ok_or_else(|| GitError::InvalidFormat("tree oid overflow".into()))?;
            if oid_end > bytes.len() {
                return Err(GitError::InvalidFormat("truncated tree object id".into()));
            }
            let oid = ObjectId::from_raw(format, &bytes[offset..oid_end])?;
            offset = oid_end;
            entries.push(TreeEntry { mode, name, oid });
        }
        Ok(Self { entries })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
            out.push(b' ');
            out.extend_from_slice(&entry.name);
            out.push(0);
            out.extend_from_slice(entry.oid.as_bytes());
        }
        out
    }
}

pub fn tree_entry_object_type(mode: u32) -> ObjectType {
    match mode {
        0o040000 => ObjectType::Tree,
        _ => ObjectType::Blob,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Vec<u8>,
    pub committer: Vec<u8>,
    pub encoding: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Commit {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("commit missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut encoding = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("tree ") {
                tree = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("parent ") {
                parents.push(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("author ") {
                author = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("committer ") {
                committer = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("encoding ") {
                encoding = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            tree: tree.ok_or_else(|| GitError::InvalidObject("commit missing tree".into()))?,
            parents,
            author: author
                .ok_or_else(|| GitError::InvalidObject("commit missing author".into()))?,
            committer: committer
                .ok_or_else(|| GitError::InvalidObject("commit missing committer".into()))?,
            encoding,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("tree {}\n", self.tree).as_bytes());
        for parent in &self.parents {
            out.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        out.extend_from_slice(b"author ");
        out.extend_from_slice(&self.author);
        out.push(b'\n');
        out.extend_from_slice(b"committer ");
        out.extend_from_slice(&self.committer);
        if let Some(encoding) = &self.encoding {
            out.extend_from_slice(b"\nencoding ");
            out.extend_from_slice(encoding);
        }
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(&self.message);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub object: ObjectId,
    pub object_type: ObjectType,
    pub name: Vec<u8>,
    pub tagger: Option<Vec<u8>>,
    pub message: Vec<u8>,
}

impl Tag {
    pub fn parse(format: ObjectFormat, bytes: &[u8]) -> Result<Self> {
        let split = bytes
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| GitError::InvalidObject("tag missing message separator".into()))?;
        let headers = std::str::from_utf8(&bytes[..split])
            .map_err(|err| GitError::InvalidObject(err.to_string()))?;
        let mut object = None;
        let mut object_type = None;
        let mut name = None;
        let mut tagger = None;
        for line in headers.lines() {
            if let Some(value) = line.strip_prefix("object ") {
                object = Some(ObjectId::from_hex(format, value)?);
            } else if let Some(value) = line.strip_prefix("type ") {
                object_type = Some(value.parse()?);
            } else if let Some(value) = line.strip_prefix("tag ") {
                name = Some(value.as_bytes().to_vec());
            } else if let Some(value) = line.strip_prefix("tagger ") {
                tagger = Some(value.as_bytes().to_vec());
            }
        }
        Ok(Self {
            object: object.ok_or_else(|| GitError::InvalidObject("tag missing object".into()))?,
            object_type: object_type
                .ok_or_else(|| GitError::InvalidObject("tag missing type".into()))?,
            name: name.ok_or_else(|| GitError::InvalidObject("tag missing name".into()))?,
            tagger,
            message: bytes[split + 2..].to_vec(),
        })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("object {}\n", self.object).as_bytes());
        out.extend_from_slice(format!("type {}\n", self.object_type.as_str()).as_bytes());
        out.extend_from_slice(b"tag ");
        out.extend_from_slice(&self.name);
        out.push(b'\n');
        if let Some(tagger) = &self.tagger {
            out.extend_from_slice(b"tagger ");
            out.extend_from_slice(tagger);
            out.push(b'\n');
        }
        out.push(b'\n');
        out.extend_from_slice(&self.message);
        out
    }
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
            .map(|entry| entry.oid.clone())
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

        let actual_checksum = git_core::digest_bytes(format, &bytes[..checksum_offset])?;
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
    let checksum = git_core::digest_bytes(format, &out)?;
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
        previous_oid = Some(oid.clone());
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

pub fn parse_framed_object(bytes: &[u8]) -> Result<EncodedObject> {
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| GitError::InvalidObject("missing object header terminator".into()))?;
    let header = std::str::from_utf8(&bytes[..nul])
        .map_err(|err| GitError::InvalidObject(err.to_string()))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| GitError::InvalidObject("missing object size".into()))?;
    let size: usize = size
        .parse()
        .map_err(|_| GitError::InvalidObject("invalid object size".into()))?;
    let body = &bytes[nul + 1..];
    if body.len() != size {
        return Err(GitError::InvalidObject(format!(
            "object declared {size} bytes, found {}",
            body.len()
        )));
    }
    Ok(EncodedObject::new(kind.parse()?, body.to_vec()))
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitConfig {
    pub sections: Vec<ConfigSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSection {
    pub name: String,
    pub subsection: Option<String>,
    pub entries: Vec<ConfigEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Option<String>,
}

impl GitConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text =
            std::str::from_utf8(bytes).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        ConfigParser::new(text).parse()
    }

    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        Self::parse(&fs::read(path)?)
    }

    /// Return the last value set for `section[.subsection].key`, or `None` if the
    /// key is unset.
    ///
    /// Matches git's "last one wins" precedence: later definitions in the file (and
    /// later files, once includes are spliced) override earlier ones. Section names
    /// and variable names are compared case-insensitively, while subsection names
    /// are matched exactly (case-sensitive), as required by the gitconfig format.
    ///
    /// A bare key with no `=` (a boolean-true variable) has `value == None`, so this
    /// returns `None` for it just as it does for an unset key; use
    /// [`GitConfig::get_bool`] to distinguish those cases.
    pub fn get(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<&str> {
        self.sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))
            .and_then(|entry| entry.value.as_deref())
    }

    /// Return every value set for `section[.subsection].key`, in file order.
    ///
    /// Multi-valued keys (the same key set several times) are preserved with their
    /// duplicates and original ordering, mirroring git's `--get-all`. Matching
    /// follows the same case rules as [`GitConfig::get`]. A bare boolean-true key
    /// contributes a `None` entry, so callers can tell `key` (present, no value)
    /// apart from `key = value`.
    pub fn get_all(
        &self,
        section: &str,
        subsection: Option<&str>,
        key: &str,
    ) -> Vec<Option<&str>> {
        self.sections
            .iter()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter())
            .filter(|entry| eq_ignore_ascii_case(&entry.key, key))
            .map(|entry| entry.value.as_deref())
            .collect()
    }

    /// Interpret the last value of `section[.subsection].key` as a git boolean.
    ///
    /// Returns `None` when the key is unset, and otherwise applies git's
    /// `git_config_bool` rules:
    /// * a bare key with no `=` is `true`;
    /// * `true`/`yes`/`on`/`1` are `true` and `false`/`no`/`off`/`0` are `false`,
    ///   compared case-insensitively;
    /// * an empty value (`key =`) is `false`;
    /// * any other value that parses as an integer is `true` when non-zero and
    ///   `false` when zero.
    ///
    /// A value that is neither a recognised keyword nor an integer yields `None`
    /// (git reports this as a "bad boolean config value" error).
    pub fn get_bool(&self, section: &str, subsection: Option<&str>, key: &str) -> Option<bool> {
        let entry = self
            .sections
            .iter()
            .rev()
            .filter(|candidate| {
                eq_ignore_ascii_case(&candidate.name, section)
                    && candidate.subsection.as_deref() == subsection
            })
            .flat_map(|candidate| candidate.entries.iter().rev())
            .find(|entry| eq_ignore_ascii_case(&entry.key, key))?;
        match &entry.value {
            // A bare key (no `=`) is boolean true.
            None => Some(true),
            Some(value) => parse_config_bool(value),
        }
    }

    pub fn repository_object_format(&self) -> Result<ObjectFormat> {
        self.get("extensions", None, "objectformat")
            .unwrap_or("sha1")
            .parse()
    }

    /// Serialise the config in git's canonical on-disk form.
    ///
    /// Section headers sit at column 0 as `[section]` or `[section "subsection"]`
    /// (subsections are quoted, with `"` and `\` backslash-escaped). Each entry is
    /// indented with a single tab and written as `key = value`, with the value
    /// quoted/escaped exactly as git would (see [`quote_config_value`]) so the
    /// result round-trips through [`GitConfig::parse`] and matches git's own output
    /// for the common cases. Bare boolean-true keys (value `None`) are written as
    /// just the key.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for section in &self.sections {
            out.extend_from_slice(b"[");
            out.extend_from_slice(section.name.as_bytes());
            if let Some(subsection) = &section.subsection {
                out.extend_from_slice(b" \"");
                out.extend_from_slice(escape_config_subsection(subsection).as_bytes());
                out.extend_from_slice(b"\"");
            }
            out.extend_from_slice(b"]\n");
            for entry in &section.entries {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(entry.key.as_bytes());
                if let Some(value) = &entry.value {
                    out.extend_from_slice(b" = ");
                    out.extend_from_slice(quote_config_value(value).as_bytes());
                }
                out.extend_from_slice(b"\n");
            }
        }
        out
    }

    /// Resolve `include`/`includeIf` directives in this already-parsed config.
    ///
    /// `base_dir` is the directory of the file these sections were parsed from;
    /// relative include paths are resolved against it. The returned config has
    /// every include directive replaced (in place, preserving order) by the
    /// parsed-and-resolved contents of the referenced file, so the existing
    /// [`GitConfig::get`]/[`GitConfig::get_bool`] precedence (last value wins)
    /// matches upstream git.
    pub fn resolve_includes(
        &self,
        base_dir: &Path,
        context: &ConfigIncludeContext,
    ) -> Result<GitConfig> {
        let mut resolved = GitConfig::default();
        splice_includes(self, base_dir, context, 0, &mut resolved.sections)?;
        Ok(resolved)
    }
}

/// Maximum depth of nested `include`/`includeIf` directives, matching git's
/// `MAX_INCLUDE_DEPTH`.
pub const CONFIG_MAX_INCLUDE_DEPTH: usize = 10;

/// Context used to evaluate conditional `includeIf` directives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigIncludeContext {
    /// Absolute path to the repository's git directory, used by `gitdir:` conditions.
    pub git_dir: Option<PathBuf>,
    /// Name of the currently checked-out branch, used by `onbranch:` conditions.
    pub current_branch: Option<String>,
}

impl ConfigIncludeContext {
    pub fn new(git_dir: Option<PathBuf>, current_branch: Option<String>) -> Self {
        Self {
            git_dir,
            current_branch,
        }
    }
}

/// Read a config file from disk and resolve its `include`/`includeIf` directives.
///
/// Missing files (including missing *included* files) are treated as empty, which
/// matches git's behaviour of silently ignoring includes that do not exist.
pub fn load_config_with_includes(
    path: &Path,
    context: &ConfigIncludeContext,
) -> Result<GitConfig> {
    let mut sections = Vec::new();
    load_config_file(path, context, 0, &mut sections)?;
    Ok(GitConfig { sections })
}

/// Read and parse a single config file, then splice its includes into `out`.
///
/// A non-existent file contributes nothing (git silently ignores it).
fn load_config_file(
    path: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let parsed = GitConfig::parse(&bytes)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    splice_includes(&parsed, base_dir, context, depth, out)
}

/// Walk the parsed sections in order, copying ordinary sections through and
/// expanding `include`/`includeIf` directives in place.
fn splice_includes(
    parsed: &GitConfig,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    if depth >= CONFIG_MAX_INCLUDE_DEPTH {
        return Err(GitError::InvalidFormat(format!(
            "exceeded maximum config include depth of {CONFIG_MAX_INCLUDE_DEPTH}"
        )));
    }
    for section in &parsed.sections {
        match include_section_kind(section) {
            Some(IncludeKind::Unconditional) => {
                expand_include_paths(section, base_dir, context, depth, out)?;
            }
            Some(IncludeKind::Conditional(condition)) => {
                if include_condition_matches(condition, base_dir, context) {
                    expand_include_paths(section, base_dir, context, depth, out)?;
                }
            }
            None => out.push(section.clone()),
        }
    }
    Ok(())
}

/// For an include section, load every `path = <p>` entry in order.
fn expand_include_paths(
    section: &ConfigSection,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    depth: usize,
    out: &mut Vec<ConfigSection>,
) -> Result<()> {
    for entry in &section.entries {
        if !eq_ignore_ascii_case(&entry.key, "path") {
            continue;
        }
        let Some(raw) = entry.value.as_deref() else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let resolved = resolve_include_path(raw, base_dir);
        load_config_file(&resolved, context, depth + 1, out)?;
    }
    Ok(())
}

enum IncludeKind<'a> {
    Unconditional,
    Conditional(&'a str),
}

/// Classify a section as an `[include]`, `[includeIf "<cond>"]`, or neither.
fn include_section_kind(section: &ConfigSection) -> Option<IncludeKind<'_>> {
    if !eq_ignore_ascii_case(&section.name, "include")
        && !eq_ignore_ascii_case(&section.name, "includeif")
    {
        return None;
    }
    // `[include]` is unconditional; `[includeIf "..."]` carries the condition in
    // its subsection. An `include` section with a subsection, or an `includeIf`
    // without one, is not a real include directive.
    match (eq_ignore_ascii_case(&section.name, "include"), &section.subsection) {
        (true, None) => Some(IncludeKind::Unconditional),
        (false, Some(condition)) => Some(IncludeKind::Conditional(condition)),
        _ => None,
    }
}

/// Resolve an include path string against `~`, the including file's directory,
/// or treat it as absolute.
fn resolve_include_path(raw: &str, base_dir: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return PathBuf::from(home).join(rest);
        }
        // No usable HOME: fall back to a relative interpretation so the lookup
        // simply misses rather than panicking.
        return base_dir.join(rest);
    }
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_dir.join(candidate)
    }
}

/// Evaluate an `includeIf` condition against the context.
fn include_condition_matches(
    condition: &str,
    base_dir: &Path,
    context: &ConfigIncludeContext,
) -> bool {
    if let Some(pattern) = condition.strip_prefix("gitdir:") {
        return gitdir_condition_matches(pattern, base_dir, context, false);
    }
    if let Some(pattern) = condition.strip_prefix("gitdir/i:") {
        return gitdir_condition_matches(pattern, base_dir, context, true);
    }
    if let Some(pattern) = condition.strip_prefix("onbranch:") {
        return match &context.current_branch {
            Some(branch) => onbranch_pattern_matches(pattern, branch),
            None => false,
        };
    }
    // `hasconfig:remote.*.url:` requires inspecting already-loaded config values
    // and is not yet implemented; treat as non-matching for now.
    false
}

/// Match a `gitdir:`/`gitdir/i:` pattern against the absolute git directory.
fn gitdir_condition_matches(
    pattern: &str,
    base_dir: &Path,
    context: &ConfigIncludeContext,
    case_insensitive: bool,
) -> bool {
    let Some(git_dir) = &context.git_dir else {
        return false;
    };
    let target = normalize_path_for_match(git_dir);

    // Expand the pattern's own prefixes, then normalise separators.
    let expanded = expand_gitdir_pattern(pattern, base_dir);
    let mut pattern = normalize_separators(&expanded);

    // A trailing slash means "match this directory and everything under it",
    // i.e. an implicit `/**` suffix.
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    // A pattern that does not contain a `/` (after expansion) is anchored to the
    // path tail in git; for our supported prefixes the pattern is always rooted,
    // so no extra handling is required here.

    glob_match(&pattern, &target, case_insensitive)
}

/// Look up `$HOME`, returning `None` when it is unset or empty.
fn home_dir() -> Option<String> {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Some(home),
        _ => None,
    }
}

/// Expand the `~/`, `./`, and bare-`**` leading forms of a `gitdir` pattern.
fn expand_gitdir_pattern(pattern: &str, base_dir: &Path) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return format!("{home}/{rest}");
        }
        return pattern.to_string();
    }
    if let Some(rest) = pattern.strip_prefix("./") {
        let base = normalize_path_for_match(base_dir);
        let base = base.trim_end_matches('/');
        return format!("{base}/{rest}");
    }
    // A pattern beginning with `**` matches anywhere; leave it as-is.
    pattern.to_string()
}

/// Normalise a path to a forward-slash string for glob comparison.
fn normalize_path_for_match(path: &Path) -> String {
    normalize_separators(&path.to_string_lossy())
}

/// Convert backslashes to forward slashes so matching is separator-agnostic.
fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

/// Match an `onbranch:` glob against a branch name. A trailing `/` means
/// "everything under this hierarchy" (implicit `/**`), as in git.
fn onbranch_pattern_matches(pattern: &str, branch: &str) -> bool {
    let mut pattern = pattern.to_string();
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    glob_match(&pattern, branch, false)
}

/// One token of a compiled glob pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobToken {
    /// A literal character that must match exactly.
    Literal(char),
    /// `?` — matches exactly one character that is not `/`.
    AnyChar,
    /// `*` — matches zero or more characters, none of which is `/`.
    Star,
    /// `**` — matches zero or more characters, including `/`.
    DoubleStar,
    /// A `[...]` character class.
    Class { negated: bool, items: Vec<ClassItem> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Single(char),
    Range(char, char),
}

/// Glob matcher supporting `*`, `?`, `[...]` character classes, and `**`.
///
/// `*` matches any run of non-`/` characters; `**` matches across `/`
/// boundaries (including none); `?` matches a single non-`/` character.
fn glob_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let (pattern, text) = if case_insensitive {
        (pattern.to_lowercase(), text.to_lowercase())
    } else {
        (pattern.to_string(), text.to_string())
    };
    let tokens = compile_glob(&pattern);
    let text_chars: Vec<char> = text.chars().collect();
    glob_match_tokens(&tokens, &text_chars)
}

/// Compile a glob string into tokens, handling `\` escapes and `[...]` classes.
fn compile_glob(pattern: &str) -> Vec<GlobToken> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut idx = 0;
    while idx < chars.len() {
        match chars[idx] {
            '*' => {
                if chars.get(idx + 1) == Some(&'*') {
                    tokens.push(GlobToken::DoubleStar);
                    idx += 2;
                } else {
                    tokens.push(GlobToken::Star);
                    idx += 1;
                }
            }
            '?' => {
                tokens.push(GlobToken::AnyChar);
                idx += 1;
            }
            '\\' => {
                if let Some(&next) = chars.get(idx + 1) {
                    tokens.push(GlobToken::Literal(next));
                    idx += 2;
                } else {
                    tokens.push(GlobToken::Literal('\\'));
                    idx += 1;
                }
            }
            '[' => {
                if let Some((token, next)) = compile_char_class(&chars, idx) {
                    tokens.push(token);
                    idx = next;
                } else {
                    // Unterminated class: treat `[` as a literal.
                    tokens.push(GlobToken::Literal('['));
                    idx += 1;
                }
            }
            other => {
                tokens.push(GlobToken::Literal(other));
                idx += 1;
            }
        }
    }
    tokens
}

/// Parse a `[...]` class beginning at `chars[start]`.
///
/// Returns the token and the index just past the closing `]`, or `None` if the
/// class is unterminated.
fn compile_char_class(chars: &[char], start: usize) -> Option<(GlobToken, usize)> {
    let mut idx = start + 1;
    let mut negated = false;
    if chars.get(idx) == Some(&'!') || chars.get(idx) == Some(&'^') {
        negated = true;
        idx += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while idx < chars.len() {
        let current = chars[idx];
        if current == ']' && !first {
            return Some((GlobToken::Class { negated, items }, idx + 1));
        }
        first = false;
        if chars.get(idx + 1) == Some(&'-')
            && chars.get(idx + 2).is_some()
            && chars.get(idx + 2) != Some(&']')
        {
            items.push(ClassItem::Range(current, chars[idx + 2]));
            idx += 3;
        } else {
            items.push(ClassItem::Single(current));
            idx += 1;
        }
    }
    None
}

/// Recursively match compiled glob tokens against the remaining text.
fn glob_match_tokens(tokens: &[GlobToken], text: &[char]) -> bool {
    let Some((token, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match token {
        GlobToken::Literal('/') => {
            // A trailing `/**` also matches the directory itself, so `foo/**`
            // matches `foo` (text already exhausted) as well as its contents.
            if text.is_empty() && rest == [GlobToken::DoubleStar] {
                return true;
            }
            matches!(text.split_first(), Some((&ch, tail)) if ch == '/' && glob_match_tokens(rest, tail))
        }
        GlobToken::Literal(expected) => {
            matches!(text.split_first(), Some((&ch, tail)) if ch == *expected && glob_match_tokens(rest, tail))
        }
        GlobToken::AnyChar => {
            matches!(text.split_first(), Some((&ch, tail)) if ch != '/' && glob_match_tokens(rest, tail))
        }
        GlobToken::Class { negated, items } => {
            matches!(text.split_first(), Some((&ch, tail))
                if ch != '/' && class_matches(items, ch) != *negated && glob_match_tokens(rest, tail))
        }
        GlobToken::Star => {
            // Match zero-or-more non-`/` characters, trying shortest first.
            if glob_match_tokens(rest, text) {
                return true;
            }
            let mut consumed = 0;
            while consumed < text.len() && text[consumed] != '/' {
                consumed += 1;
                if glob_match_tokens(rest, &text[consumed..]) {
                    return true;
                }
            }
            false
        }
        GlobToken::DoubleStar => {
            match rest.split_first() {
                // `**/<rest>` (a full path-component wildcard): match zero or
                // more complete `component/` units. So `a/**/b` matches `a/b`,
                // `a/x/b`, `a/x/y/b`, and a leading `**/foo` matches `foo` at
                // any depth.
                Some((GlobToken::Literal('/'), after_slash)) => {
                    // Zero directories: the `**/` collapses away entirely.
                    if glob_match_tokens(after_slash, text) {
                        return true;
                    }
                    // One or more directories: consume up to and including the
                    // next `/`, then retry the whole `**/...` against the rest.
                    let mut consumed = 0;
                    while consumed < text.len() {
                        let ch = text[consumed];
                        consumed += 1;
                        if ch == '/' && glob_match_tokens(tokens, &text[consumed..]) {
                            return true;
                        }
                    }
                    false
                }
                // Trailing `**` or `**` before a non-slash: match any run of
                // characters, including `/` and including none.
                _ => {
                    if glob_match_tokens(rest, text) {
                        return true;
                    }
                    for consumed in 1..=text.len() {
                        if glob_match_tokens(rest, &text[consumed..]) {
                            return true;
                        }
                    }
                    false
                }
            }
        }
    }
}

fn class_matches(items: &[ClassItem], ch: char) -> bool {
    items.iter().any(|item| match item {
        ClassItem::Single(value) => *value == ch,
        ClassItem::Range(lo, hi) => *lo <= ch && ch <= *hi,
    })
}

/// Character-level parser for the gitconfig file format.
///
/// This mirrors git's own `git_parse_source`: it scans the input as a stream of
/// characters rather than independent lines, because both line continuations
/// (a trailing `\`) and quoted strings (in values *and* subsection headers) may
/// span physical lines. Section/variable names are lower-cased (they are
/// case-insensitive); subsection names in the quoted form keep their case, while
/// the deprecated dotted form lower-cases the subsection.
struct ConfigParser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    /// 1-based physical line number, advanced on every consumed `\n`.
    line: usize,
}

impl<'a> ConfigParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            line: 1,
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    /// Consume and return the next character, tracking line numbers.
    fn bump(&mut self) -> Option<char> {
        let ch = self.chars.next();
        if ch == Some('\n') {
            self.line += 1;
        }
        ch
    }

    fn err(&self, message: impl std::fmt::Display) -> GitError {
        GitError::InvalidFormat(format!("config line {}: {message}", self.line))
    }

    /// Skip spaces and tabs (but never newlines), returning the next char if any.
    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.bump();
        }
    }

    fn parse(mut self) -> Result<GitConfig> {
        let mut config = GitConfig::default();
        let mut current: Option<usize> = None;
        loop {
            self.skip_blanks();
            match self.peek() {
                None => break,
                Some('\n') => {
                    self.bump();
                }
                Some('#') | Some(';') => self.skip_to_eol(),
                Some('[') => {
                    let section = self.parse_section_header()?;
                    config.sections.push(section);
                    current = Some(config.sections.len() - 1);
                }
                Some(ch) if ch.is_ascii_alphabetic() => {
                    let entry = self.parse_entry()?;
                    let Some(idx) = current else {
                        return Err(self.err("variable definition appears before a section"));
                    };
                    config.sections[idx].entries.push(entry);
                }
                Some(ch) => {
                    return Err(self.err(format!("unexpected character {ch:?}")));
                }
            }
        }
        Ok(config)
    }

    /// Consume the rest of the current physical line, including its terminator.
    fn skip_to_eol(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == '\n' {
                self.bump();
                break;
            }
            self.bump();
        }
    }

    /// Parse a `[section]`, `[section "subsection"]`, or deprecated
    /// `[section.subsection]` header. The leading `[` is the next character.
    fn parse_section_header(&mut self) -> Result<ConfigSection> {
        self.bump(); // consume '['
        // Section name: alphanumeric, '-', and '.' (the dotted-subsection form).
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                name.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        // Deprecated dotted form: `[section.subsection]`. The subsection runs to
        // the first '.', everything after is the (case-insensitive) subsection.
        if let Some((head, rest)) = name.split_once('.') {
            let subsection = rest.to_string();
            let head = head.to_string();
            self.skip_blanks();
            match self.bump() {
                Some(']') => {}
                _ => return Err(self.err("missing ']' after dotted section header")),
            }
            if !is_config_name(&head) {
                return Err(self.err(format!("invalid section name {head}")));
            }
            // Subsection in the dotted form is lower-cased by git.
            return Ok(ConfigSection {
                name: head.to_ascii_lowercase(),
                subsection: Some(subsection.to_ascii_lowercase()),
                entries: Vec::new(),
            });
        }
        if !is_config_name(&name) {
            return Err(self.err(format!("invalid section name {name}")));
        }
        // Either a closing ']' or whitespace followed by a quoted subsection.
        match self.peek() {
            Some(']') => {
                self.bump();
                Ok(ConfigSection {
                    name: name.to_ascii_lowercase(),
                    subsection: None,
                    entries: Vec::new(),
                })
            }
            Some(' ') | Some('\t') => {
                self.skip_blanks();
                if self.peek() != Some('"') {
                    return Err(self.err("expected quoted subsection name"));
                }
                let subsection = self.parse_subsection_name()?;
                self.skip_blanks();
                match self.bump() {
                    Some(']') => {}
                    _ => return Err(self.err("missing ']' after subsection name")),
                }
                Ok(ConfigSection {
                    name: name.to_ascii_lowercase(),
                    // Subsection names are case-sensitive in the quoted form.
                    subsection: Some(subsection),
                    entries: Vec::new(),
                })
            }
            _ => Err(self.err("malformed section header")),
        }
    }

    /// Parse the contents of a quoted subsection name (the opening `"` is next).
    ///
    /// Only `\\` and `\"` are escapes here; any other `\<char>` keeps the literal
    /// character (dropping the backslash), and `\n`/`\t` are NOT interpreted.
    fn parse_subsection_name(&mut self) -> Result<String> {
        self.bump(); // consume opening '"'
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.err("unterminated subsection name")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    None | Some('\n') => {
                        return Err(self.err("unterminated subsection name"));
                    }
                    Some(other) => out.push(other),
                },
                Some(other) => out.push(other),
            }
        }
    }

    /// Parse a `name` or `name = value` entry. The first character of the name is
    /// the next character.
    fn parse_entry(&mut self) -> Result<ConfigEntry> {
        let mut key = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                key.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        if !is_config_name(&key) {
            return Err(self.err(format!("invalid variable name {key}")));
        }
        self.skip_blanks();
        match self.peek() {
            // Bare variable: boolean true. Nothing but a comment or EOL may follow.
            None => Ok(ConfigEntry {
                key: key.to_ascii_lowercase(),
                value: None,
            }),
            Some('\n') => {
                self.bump();
                Ok(ConfigEntry {
                    key: key.to_ascii_lowercase(),
                    value: None,
                })
            }
            Some('=') => {
                self.bump();
                let value = self.parse_value()?;
                Ok(ConfigEntry {
                    key: key.to_ascii_lowercase(),
                    value: Some(value),
                })
            }
            Some(ch) => Err(self.err(format!("expected '=' after variable name, found {ch:?}"))),
        }
    }

    /// Parse a variable value after the `=`.
    ///
    /// Handles: leading/trailing whitespace trimming (outside quotes), double
    /// quotes that preserve spaces, the escapes `\n \t \b \" \\`, line
    /// continuation via a trailing `\`, and inline `#`/`;` comments (outside
    /// quotes). Quoted runs and unquoted runs may be mixed within one value.
    fn parse_value(&mut self) -> Result<String> {
        let mut out = String::new();
        // Number of trailing whitespace chars currently buffered in `out` that
        // should be dropped if the value ends here (outside quotes).
        let mut trailing_ws = 0usize;
        let mut leading = true;
        let mut in_quotes = false;
        loop {
            match self.peek() {
                None => break,
                Some('\n') if !in_quotes => {
                    self.bump();
                    break;
                }
                Some('\n') => return Err(self.err("newline inside quoted value")),
                Some('"') => {
                    self.bump();
                    in_quotes = !in_quotes;
                    leading = false;
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        // Line continuation: backslash immediately before a newline.
                        Some('\n') => {}
                        Some('\r') if self.peek() == Some('\n') => {
                            self.bump();
                        }
                        Some('n') => {
                            out.push('\n');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('t') => {
                            out.push('\t');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('b') => {
                            out.push('\u{0008}');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('"') => {
                            out.push('"');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some('\\') => {
                            out.push('\\');
                            trailing_ws = 0;
                            leading = false;
                        }
                        Some(other) => {
                            return Err(self.err(format!("invalid escape sequence \\{other}")));
                        }
                        // A backslash right at end-of-input is a continuation with
                        // nothing to continue onto; git tolerates this.
                        None => break,
                    }
                }
                // Comments terminate an unquoted value.
                Some('#') | Some(';') if !in_quotes => {
                    self.skip_to_eol();
                    break;
                }
                Some(ch @ (' ' | '\t' | '\r')) if !in_quotes => {
                    self.bump();
                    if leading {
                        // Drop leading whitespace entirely.
                    } else {
                        out.push(ch);
                        trailing_ws += 1;
                    }
                }
                Some(ch) => {
                    self.bump();
                    out.push(ch);
                    trailing_ws = 0;
                    leading = false;
                }
            }
        }
        if in_quotes {
            return Err(self.err("unterminated quoted value"));
        }
        // Trim trailing unquoted whitespace that was buffered.
        out.truncate(out.len() - trailing_ws);
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

/// Quote and escape a config value the way git's writer does.
///
/// The value is wrapped in double quotes only when it begins or ends with a space
/// or contains a `#` or `;` (which would otherwise start a comment). Independently
/// of quoting, `\` becomes `\\`, `"` becomes `\"`, tab becomes `\t`, and newline
/// becomes `\n`; other characters (including backspace) are emitted verbatim, just
/// as git does. The result always round-trips back through the parser to the
/// original value.
fn quote_config_value(value: &str) -> String {
    let needs_quotes = value.starts_with(' ')
        || value.ends_with(' ')
        || value.bytes().any(|byte| matches!(byte, b'#' | b';'));
    let mut out = String::new();
    if needs_quotes {
        out.push('"');
    }
    for ch in value.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

/// Escape a subsection name for a `[section "subsection"]` header.
///
/// Only `\` and `"` are escaped (to `\\` and `\"`); all other characters are
/// emitted verbatim, matching git's section-header writer. (Newlines and tabs
/// cannot legally appear in a subsection name.)
fn escape_config_subsection(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// Parse a string as a git boolean, returning `None` if it is not a valid boolean.
///
/// Implements git's `git_config_bool` rules so the CLI can share one source of
/// truth. The keywords `true`/`yes`/`on`/`1` are `true` and `false`/`no`/`off`/`0`
/// are `false` (case-insensitive). An empty string is `false`. Any other value
/// that parses as an integer (see [`parse_config_int`]) is `true` when non-zero
/// and `false` when zero; everything else returns `None`.
///
/// Note: a *bare* key with no `=` is boolean `true`, but that is represented as a
/// `None` value at the [`ConfigEntry`] level and handled by [`GitConfig::get_bool`];
/// this function only classifies an explicit value string.
pub fn parse_config_bool(value: &str) -> Option<bool> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
        || trimmed == "1"
    {
        return Some(true);
    }
    if trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed == "0"
        || trimmed.is_empty()
    {
        return Some(false);
    }
    // Fall back to git's bool-from-int behaviour: any integer is true unless zero.
    parse_config_int(trimmed).map(|number| number != 0)
}

/// Parse a string as a git integer, returning `None` if it is not a valid integer.
///
/// Implements git's `git_parse_long`/unit handling so the CLI can share one source
/// of truth. A single trailing `k`/`m`/`g` suffix (case-insensitive) multiplies by
/// 1024, 1024², or 1024³ respectively. Decimal (optionally signed), hexadecimal
/// (`0x`), and octal (`0`-prefixed) bases are accepted, just like `strtol`.
/// Overflow on the multiplication or the base parse yields `None`.
pub fn parse_config_int(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, multiplier) = match trimmed.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&trimmed[..trimmed.len() - 1], 1024_i64),
        Some(b'm' | b'M') => (&trimmed[..trimmed.len() - 1], 1024_i64 * 1024),
        Some(b'g' | b'G') => (&trimmed[..trimmed.len() - 1], 1024_i64 * 1024 * 1024),
        _ => (trimmed, 1_i64),
    };
    // git requires the unit suffix to immediately follow the digits (no space),
    // so `digits` is parsed as-is rather than re-trimmed.
    parse_c_long(digits)?.checked_mul(multiplier)
}

/// Parse an optionally-signed integer in decimal, hex (`0x`), or octal (`0`)
/// notation, mirroring C's `strtol` with base 0 as git uses for config integers.
fn parse_c_long(text: &str) -> Option<i64> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if rest.is_empty() {
        return None;
    }
    let magnitude = if let Some(hex) = rest
        .strip_prefix("0x")
        .or_else(|| rest.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16).ok()?
    } else if rest.len() > 1 && rest.starts_with('0') {
        i64::from_str_radix(rest, 8).ok()?
    } else {
        rest.parse::<i64>().ok()?
    };
    if negative {
        magnitude.checked_neg()
    } else {
        Some(magnitude)
    }
}

/// The result of interpreting a config value with git's `--bool-or-int` typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigBoolOrInt {
    /// The value is a recognised boolean keyword (`true`/`false`/`yes`/...).
    Bool(bool),
    /// The value is an integer (possibly with a `k`/`m`/`g` unit suffix).
    Int(i64),
}

/// Parse a string with git's `--bool-or-int` typing rules.
///
/// A value that is a boolean *keyword* (`true`/`false`/`yes`/`no`/`on`/`off`, or an
/// empty string) is returned as [`ConfigBoolOrInt::Bool`]; otherwise an integer
/// value (see [`parse_config_int`]) is returned as [`ConfigBoolOrInt::Int`]. The
/// bare numbers `0` and `1` are treated as integers, matching git. An empty string
/// is `Bool(false)` (as git treats `key =`). Anything that is neither a boolean
/// keyword nor an integer returns `None`.
pub fn parse_config_bool_or_int(value: &str) -> Option<ConfigBoolOrInt> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("true")
        || trimmed.eq_ignore_ascii_case("yes")
        || trimmed.eq_ignore_ascii_case("on")
    {
        return Some(ConfigBoolOrInt::Bool(true));
    }
    if trimmed.eq_ignore_ascii_case("false")
        || trimmed.eq_ignore_ascii_case("no")
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.is_empty()
    {
        return Some(ConfigBoolOrInt::Bool(false));
    }
    parse_config_int(trimmed).map(ConfigBoolOrInt::Int)
}

fn is_config_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn eq_ignore_ascii_case(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
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
                    },
                    ConfigEntry {
                        key: "filemode".into(),
                        value: Some("true".into()),
                    },
                    ConfigEntry {
                        key: "bare".into(),
                        value: Some(if bare { "true" } else { "false" }.into()),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub version: u32,
    pub entries: Vec<IndexEntry>,
    pub extensions: Vec<u8>,
    pub checksum: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub ctime_seconds: u32,
    pub ctime_nanoseconds: u32,
    pub mtime_seconds: u32,
    pub mtime_nanoseconds: u32,
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: ObjectId,
    pub flags: u16,
    pub flags_extended: u16,
    pub path: Vec<u8>,
}

impl Index {
    pub fn parse(bytes: &[u8], format: ObjectFormat) -> Result<Self> {
        let hash_len = format.raw_len();
        if bytes.len() < 12 + hash_len {
            return Err(GitError::InvalidFormat("index header too short".into()));
        }
        let checksum_offset = bytes.len() - hash_len;
        let actual_checksum = git_core::digest_bytes(format, &bytes[..checksum_offset])?;
        let checksum = ObjectId::from_raw(format, &bytes[checksum_offset..])?;
        if actual_checksum != checksum {
            return Err(GitError::InvalidFormat(format!(
                "index checksum mismatch: expected {checksum}, got {actual_checksum}"
            )));
        }
        if &bytes[..4] != b"DIRC" {
            return Err(GitError::InvalidFormat("missing DIRC signature".into()));
        }
        let version = u32_be(&bytes[4..8]);
        if !(2..=4).contains(&version) {
            return Err(GitError::Unsupported(format!("index version {version}")));
        }
        let count = u32_be(&bytes[8..12]) as usize;
        let mut offset = 12;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let entry_header_len = 40 + hash_len + 2;
            if checksum_offset.saturating_sub(offset) < entry_header_len {
                return Err(GitError::InvalidFormat("truncated index entry".into()));
            }
            let start = offset;
            let oid_start = offset + 40;
            let oid_end = oid_start + hash_len;
            let oid = ObjectId::from_raw(format, &bytes[oid_start..oid_end])?;
            let flags = u16_be(&bytes[oid_end..oid_end + 2]);
            offset = oid_end + 2;
            let flags_extended = if flags & INDEX_FLAG_EXTENDED != 0 {
                if checksum_offset.saturating_sub(offset) < 2 {
                    return Err(GitError::InvalidFormat(
                        "truncated index extended flags".into(),
                    ));
                }
                let flags_extended = u16_be(&bytes[offset..offset + 2]);
                offset += 2;
                flags_extended
            } else {
                0
            };
            let path = if version == 4 {
                let previous_path = entries
                    .last()
                    .map(|entry: &IndexEntry| entry.path.as_slice())
                    .unwrap_or(&[]);
                let strip_len =
                    decode_index_v4_path_strip_len(bytes, &mut offset, checksum_offset)?;
                if strip_len > previous_path.len() {
                    return Err(GitError::InvalidFormat(
                        "index v4 path compression removes too much prefix".into(),
                    ));
                }
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let mut path = previous_path[..previous_path.len() - strip_len].to_vec();
                path.extend_from_slice(&bytes[path_start..offset]);
                offset += 1;
                path
            } else {
                let path_start = offset;
                while bytes.get(offset).copied() != Some(0) {
                    offset += 1;
                    if offset >= checksum_offset {
                        return Err(GitError::InvalidFormat("unterminated index path".into()));
                    }
                }
                let path = bytes[path_start..offset].to_vec();
                offset += 1;
                while (offset - start) % 8 != 0 {
                    offset += 1;
                    if offset > checksum_offset {
                        return Err(GitError::InvalidFormat("truncated index padding".into()));
                    }
                }
                path
            };
            entries.push(IndexEntry {
                ctime_seconds: u32_be(&bytes[start..start + 4]),
                ctime_nanoseconds: u32_be(&bytes[start + 4..start + 8]),
                mtime_seconds: u32_be(&bytes[start + 8..start + 12]),
                mtime_nanoseconds: u32_be(&bytes[start + 12..start + 16]),
                dev: u32_be(&bytes[start + 16..start + 20]),
                ino: u32_be(&bytes[start + 20..start + 24]),
                mode: u32_be(&bytes[start + 24..start + 28]),
                uid: u32_be(&bytes[start + 28..start + 32]),
                gid: u32_be(&bytes[start + 32..start + 36]),
                size: u32_be(&bytes[start + 36..start + 40]),
                oid,
                flags,
                flags_extended,
                path,
            });
        }
        Ok(Self {
            version,
            entries,
            extensions: bytes[offset..checksum_offset].to_vec(),
            checksum: Some(checksum),
        })
    }

    pub fn parse_v2_sha1(bytes: &[u8]) -> Result<Self> {
        Self::parse(bytes, ObjectFormat::Sha1)
    }

    pub fn write_v2_sha1(&self) -> Result<Vec<u8>> {
        if self.version != 2 {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2".into(),
            ));
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0)
        {
            return Err(GitError::Unsupported(
                "index v2 writer cannot emit extended flags".into(),
            ));
        }
        self.write_sha1()
    }

    pub fn write_sha1(&self) -> Result<Vec<u8>> {
        self.write(ObjectFormat::Sha1)
    }

    pub fn write(&self, format: ObjectFormat) -> Result<Vec<u8>> {
        if !(2..=4).contains(&self.version) {
            return Err(GitError::Unsupported(
                "canonical writer currently emits index v2/v3/v4".into(),
            ));
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        let mut previous_path = Vec::new();
        for entry in &self.entries {
            let start = out.len();
            out.extend_from_slice(&entry.ctime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.ctime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_seconds.to_be_bytes());
            out.extend_from_slice(&entry.mtime_nanoseconds.to_be_bytes());
            out.extend_from_slice(&entry.dev.to_be_bytes());
            out.extend_from_slice(&entry.ino.to_be_bytes());
            out.extend_from_slice(&entry.mode.to_be_bytes());
            out.extend_from_slice(&entry.uid.to_be_bytes());
            out.extend_from_slice(&entry.gid.to_be_bytes());
            out.extend_from_slice(&entry.size.to_be_bytes());
            if entry.oid.format() != format {
                return Err(GitError::Unsupported(format!(
                    "index writer expects {} ids",
                    format.name()
                )));
            }
            out.extend_from_slice(entry.oid.as_bytes());
            let has_extended_flags =
                entry.flags & INDEX_FLAG_EXTENDED != 0 || entry.flags_extended != 0;
            if has_extended_flags && self.version < 3 {
                return Err(GitError::Unsupported(
                    "index extended flags require version 3".into(),
                ));
            }
            let flags = if has_extended_flags {
                entry.flags | INDEX_FLAG_EXTENDED
            } else {
                entry.flags & !INDEX_FLAG_EXTENDED
            };
            out.extend_from_slice(&flags.to_be_bytes());
            if has_extended_flags {
                out.extend_from_slice(&entry.flags_extended.to_be_bytes());
            }
            if self.version == 4 {
                let common_prefix_len = common_prefix_len(&previous_path, &entry.path);
                let strip_len = previous_path.len() - common_prefix_len;
                encode_index_v4_path_strip_len(strip_len, &mut out);
                out.extend_from_slice(&entry.path[common_prefix_len..]);
                out.push(0);
                previous_path = entry.path.clone();
            } else {
                out.extend_from_slice(&entry.path);
                out.push(0);
                while (out.len() - start) % 8 != 0 {
                    out.push(0);
                }
            }
        }
        out.extend_from_slice(&self.extensions);
        let checksum = git_core::digest_bytes(format, &out)?;
        out.extend_from_slice(checksum.as_bytes());
        Ok(out)
    }
}

const INDEX_FLAG_EXTENDED: u16 = 0x4000;

fn decode_index_v4_path_strip_len(
    bytes: &[u8],
    offset: &mut usize,
    checksum_offset: usize,
) -> Result<usize> {
    let Some(first) = bytes.get(*offset).copied() else {
        return Err(GitError::InvalidFormat(
            "truncated index v4 path compression".into(),
        ));
    };
    *offset += 1;
    let mut value = (first & 0x7f) as usize;
    let mut byte = first;
    while byte & 0x80 != 0 {
        if *offset >= checksum_offset {
            return Err(GitError::InvalidFormat(
                "truncated index v4 path compression".into(),
            ));
        }
        byte = bytes[*offset];
        *offset += 1;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add((byte & 0x7f) as usize))
            .ok_or_else(|| GitError::InvalidFormat("index v4 path compression overflow".into()))?;
    }
    Ok(value)
}

fn encode_index_v4_path_strip_len(strip_len: usize, out: &mut Vec<u8>) {
    let mut bytes = Vec::new();
    bytes.push((strip_len & 0x7f) as u8);
    let mut value = strip_len >> 7;
    while value != 0 {
        value -= 1;
        bytes.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    for byte in bytes.iter().rev() {
        out.push(*byte);
    }
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

fn u32_be(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn u16_be(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
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
    fn framed_object_round_trips() {
        let object = EncodedObject::new(ObjectType::Blob, b"hello\n".to_vec());
        assert_eq!(parse_framed_object(&object.framed_bytes()).unwrap(), object);
    }

    #[test]
    fn tree_round_trips_entries() {
        let blob = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "ce013625030ba8dba906f756967f9e9ca394464a",
        )
        .unwrap();
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: 0o100644,
                name: b"hello.txt".to_vec(),
                oid: blob,
            }],
        };
        assert_eq!(
            Tree::parse(ObjectFormat::Sha1, &tree.write()).unwrap(),
            tree
        );
    }

    #[test]
    fn reftable_empty_table_round_trips() {
        let bytes = Reftable::write_ref_only(ObjectFormat::Sha1, 1, 1, &[]).unwrap();
        let table = Reftable::parse(&bytes).unwrap();

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
                    target: tag.clone(),
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

        let bytes = Reftable::write_ref_only(ObjectFormat::Sha1, 7, 7, &refs).unwrap();
        let table = Reftable::parse(&bytes).unwrap();

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
        .unwrap();
        let refs = vec![ReftableRefRecord {
            name: "refs/heads/main".into(),
            update_index: 3,
            value: ReftableRefValue::Direct(oid.clone()),
        }];

        let bytes = Reftable::write_ref_only(ObjectFormat::Sha256, 3, 3, &refs).unwrap();
        let table = Reftable::parse(&bytes).unwrap();

        assert_eq!(table.header.version, ReftableVersion::V2);
        assert_eq!(table.header.object_format, ObjectFormat::Sha256);
        assert_eq!(table.refs[0].value, ReftableRefValue::Direct(oid));
    }

    #[test]
    fn upstream_git_reads_rust_written_minimal_reftable() {
        let root = unique_temp_dir("reftable-upstream");
        fs::create_dir_all(&root).expect("create temp repo");
        let result = (|| {
            run_success("git", &root, &["init", "-q"]);
            let oid = run_success_with_stdin(
                "git",
                &root,
                &["hash-object", "-w", "--stdin"],
                b"payload\n",
            );
            let oid = String::from_utf8(oid).expect("oid is utf8");
            let oid = ObjectId::from_hex(ObjectFormat::Sha1, oid.trim()).unwrap();
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
                    value: ReftableRefValue::Direct(oid.clone()),
                }],
            )
            .unwrap();
            fs::write(reftable_dir.join(table_name), table).expect("write reftable");
            fs::write(reftable_dir.join("tables.list"), format!("{table_name}\n"))
                .expect("write tables.list");

            let output = run_success("git", &root, &["show-ref"]);
            assert_eq!(
                String::from_utf8(output).expect("show-ref output is utf8"),
                format!("{oid} refs/heads/main\n")
            );
        })();
        let _ = fs::remove_dir_all(&root);
        result
    }

    #[test]
    fn commit_round_trips_headers_and_message() {
        let tree = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        )
        .unwrap();
        let commit = Commit {
            tree,
            parents: Vec::new(),
            author: b"A U Thor <a@example.invalid> 0 +0000".to_vec(),
            committer: b"C O Mitter <c@example.invalid> 0 +0000".to_vec(),
            encoding: Some(b"ISO-8859-1".to_vec()),
            message: b"subject\n\nbody\n".to_vec(),
        };
        assert_eq!(
            Commit::parse(ObjectFormat::Sha1, &commit.write()).unwrap(),
            commit
        );
    }

    #[test]
    fn parses_commit_graph_core_chunks() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree.clone(),
                Vec::new(),
                1,
                1,
            ),
            (
                oid("2222222222222222222222222222222222222222"),
                tree.clone(),
                vec![0],
                2,
                2,
            ),
            (
                oid("3333333333333333333333333333333333333333"),
                tree.clone(),
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

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.format, ObjectFormat::Sha1);
        assert_eq!(parsed.base_graph_count, 0);
        assert_eq!(parsed.commits.len(), 4);
        let merge = parsed.find(&commits[3].0).unwrap();
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
                    tree: tree.clone(),
                    parents: vec![main.clone(), side.clone()],
                    generation: 3,
                    commit_time: 30,
                },
                CommitGraphWriteEntry {
                    oid: base.clone(),
                    tree: tree.clone(),
                    parents: Vec::new(),
                    generation: 1,
                    commit_time: 10,
                },
                CommitGraphWriteEntry {
                    oid: main.clone(),
                    tree: tree.clone(),
                    parents: vec![base.clone()],
                    generation: 2,
                    commit_time: 20,
                },
                CommitGraphWriteEntry {
                    oid: side.clone(),
                    tree,
                    parents: vec![base.clone()],
                    generation: 2,
                    commit_time: 21,
                },
            ],
        )
        .unwrap();

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.commits.len(), 4);
        assert_eq!(parsed.find(&base).unwrap().parents, Vec::<u32>::new());
        assert_eq!(parsed.find(&main).unwrap().parents, vec![0]);
        assert_eq!(parsed.find(&side).unwrap().parents, vec![0]);
        assert_eq!(parsed.find(&merge).unwrap().parents, vec![1, 2]);
        assert_eq!(parsed.find(&merge).unwrap().generation, 3);
        assert_eq!(parsed.find(&merge).unwrap().commit_time, 30);
    }

    #[test]
    fn parses_commit_graph_bloom_filters() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let commits = vec![
            (
                oid("1111111111111111111111111111111111111111"),
                tree.clone(),
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

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
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
                tree.clone(),
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

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
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

        let parsed = CommitGraph::parse(&bytes, ObjectFormat::Sha1).unwrap();
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
                (*b"CDAT", commit_graph_cdat(&[commit.clone()]).0),
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
                .unwrap()
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
    fn tag_round_trips_headers_and_message() {
        let object = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "e7556fb3ba7b8f5b1f4772180772a4d6a7323e15",
        )
        .unwrap();
        let tag = Tag {
            object,
            object_type: ObjectType::Commit,
            name: b"v1.0".to_vec(),
            tagger: Some(b"Example User <example@example.invalid> 0 +0000".to_vec()),
            message: b"release\n".to_vec(),
        };
        assert_eq!(Tag::parse(ObjectFormat::Sha1, &tag.write()).unwrap(), tag);
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

        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
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
        .unwrap();
        let reference = ObjectId::from_hex(
            ObjectFormat::Sha256,
            "2222222222222222222222222222222222222222222222222222222222222222",
        )
        .unwrap();
        let bytes = format!(
            "# v3 git bundle\n@object-format=sha256\n@filter=blob:none\n-{prerequisite} base\n{reference} refs/heads/main\n\n"
        )
        .into_bytes();

        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
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
        let sha1 = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n").unwrap();
        let sha256 = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n").unwrap();

        let sha1_bytes = format!("# v2 git bundle\n{sha1} refs/heads/main\n\nPACK").into_bytes();
        let sha1_bundle = Bundle::parse_standalone(&sha1_bytes).unwrap();
        assert_eq!(sha1_bundle.format, ObjectFormat::Sha1);
        assert_eq!(sha1_bundle.references[0].oid, sha1);

        let sha256_bytes =
            format!("# v3 git bundle\n@object-format=sha256\n{sha256} refs/heads/main\n\nPACK")
                .into_bytes();
        let sha256_bundle = Bundle::parse_standalone(&sha256_bytes).unwrap();
        assert_eq!(sha256_bundle.format, ObjectFormat::Sha256);
        assert_eq!(sha256_bundle.references[0].oid, sha256);
    }

    #[test]
    fn writes_bundle_v2_header_and_pack() {
        let prerequisite =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"base\n").unwrap();
        let reference =
            git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n").unwrap();
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

        let bytes = bundle.write().unwrap();
        let expected = format!(
            "# v2 git bundle\n-{prerequisite} base comment\n{reference} refs/heads/main\n\n"
        )
        .into_bytes()
        .into_iter()
        .chain(b"PACKv2".iter().copied())
        .collect::<Vec<_>>();
        assert_eq!(bytes, expected);
        assert_eq!(Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap(), bundle);
    }

    #[test]
    fn writes_bundle_v3_sha256_object_format_capability() {
        let oid = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n").unwrap();
        let bundle = Bundle {
            version: 3,
            format: ObjectFormat::Sha256,
            capabilities: vec![BundleCapability {
                key: "filter".into(),
                value: Some(b"blob:none".to_vec()),
            }],
            prerequisites: Vec::new(),
            references: vec![BundleReference {
                oid: oid.clone(),
                name: "refs/heads/main".into(),
            }],
            pack: b"PACKv3".to_vec(),
        };

        let bytes = bundle.write().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("# v3 git bundle\n@filter=blob:none\n@object-format=sha256\n"));
        let parsed = Bundle::parse(&bytes, ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.format, ObjectFormat::Sha256);
        assert_eq!(parsed.references[0].oid, oid);
        assert_eq!(parsed.pack, b"PACKv3");
    }

    #[test]
    fn rejects_bad_bundle_write_inputs() {
        let sha1 = git_core::object_id_for_bytes(ObjectFormat::Sha1, "blob", b"tip\n").unwrap();
        let sha256 = git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"tip\n").unwrap();
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

    #[test]
    fn config_parses_sections_values_and_comments() {
        let config = GitConfig::parse(
            br#"
[core]
    filemode = true
    bare = false ; comment
[remote "origin"]
    url = "https://example.invalid/repo.git"
    fetch = +refs/heads/*:refs/remotes/origin/*
[feature]
    enabled
"#,
        )
        .unwrap();
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        assert_eq!(config.get_bool("core", None, "bare"), Some(false));
        assert_eq!(
            config.get("remote", Some("origin"), "url"),
            Some("https://example.invalid/repo.git")
        );
        assert_eq!(config.get_bool("feature", None, "enabled"), Some(true));
    }

    #[test]
    fn config_reports_repository_object_format() {
        let config = GitConfig::parse(b"[extensions]\n\tobjectformat = sha256\n").unwrap();
        assert_eq!(
            config.repository_object_format().unwrap(),
            ObjectFormat::Sha256
        );
    }

    #[test]
    fn config_canonical_writer_round_trips() {
        let config = GitConfig {
            sections: vec![ConfigSection {
                name: "remote".into(),
                subsection: Some("origin repo".into()),
                entries: vec![ConfigEntry {
                    key: "url".into(),
                    value: Some("https://example.invalid/repo.git".into()),
                }],
            }],
        };
        let parsed = GitConfig::parse(&config.to_canonical_bytes()).unwrap();
        assert_eq!(parsed, config);
    }

    // ----- gitconfig format compliance tests -----

    /// Convenience: parse and fetch the single `core.x` value (panicking on parse
    /// errors is fine here because each input is a known-good fixture).
    fn parse_core_x(input: &str) -> Option<String> {
        GitConfig::parse(input.as_bytes())
            .unwrap()
            .get("core", None, "x")
            .map(str::to_string)
    }

    #[test]
    fn config_section_name_is_case_insensitive() {
        let config = GitConfig::parse(b"[Core]\n\tBar = value\n").unwrap();
        assert_eq!(config.get("core", None, "bar"), Some("value"));
        assert_eq!(config.get("CORE", None, "BAR"), Some("value"));
        // Stored names are lower-cased.
        assert_eq!(config.sections[0].name, "core");
        assert_eq!(config.sections[0].entries[0].key, "bar");
    }

    #[test]
    fn config_subsection_name_is_case_sensitive() {
        let config = GitConfig::parse(b"[remote \"Origin\"]\n\turl = x\n").unwrap();
        assert_eq!(config.get("remote", Some("Origin"), "url"), Some("x"));
        // Case-mismatched subsection must not match.
        assert_eq!(config.get("remote", Some("origin"), "url"), None);
    }

    #[test]
    fn config_subsection_accepts_escaped_quote_and_backslash() {
        // [remote "with\"quote"] -> subsection is with"quote
        let config = GitConfig::parse(b"[remote \"with\\\"quote\"]\n\turl = x\n").unwrap();
        assert_eq!(config.sections[0].subsection.as_deref(), Some("with\"quote"));
        assert_eq!(config.get("remote", Some("with\"quote"), "url"), Some("x"));

        // [remote "a\\b"] -> subsection is a\b
        let config = GitConfig::parse(b"[remote \"a\\\\b\"]\n\turl = y\n").unwrap();
        assert_eq!(config.sections[0].subsection.as_deref(), Some("a\\b"));
    }

    #[test]
    fn config_subsection_unknown_escape_keeps_literal_char() {
        // In a subsection only \\ and \" are real escapes; \n is a literal "n",
        // NOT a newline (unlike a value).
        let config = GitConfig::parse(b"[remote \"a\\nb\"]\n\turl = x\n").unwrap();
        assert_eq!(config.sections[0].subsection.as_deref(), Some("anb"));
    }

    #[test]
    fn config_dotted_subsection_is_case_insensitive() {
        // Deprecated [section.subsection] form: subsection is lower-cased.
        let config = GitConfig::parse(b"[core.Sub]\n\tbar = x\n").unwrap();
        assert_eq!(config.sections[0].name, "core");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("sub"));
        assert_eq!(config.get("core", Some("sub"), "bar"), Some("x"));
        // The original (mixed) case must not match.
        assert_eq!(config.get("core", Some("Sub"), "bar"), None);
    }

    #[test]
    fn config_dotted_subsection_keeps_inner_dots() {
        // Everything after the first dot is the subsection, dots and all.
        let config = GitConfig::parse(b"[a.b.c]\n\tk = v\n").unwrap();
        assert_eq!(config.sections[0].name, "a");
        assert_eq!(config.sections[0].subsection.as_deref(), Some("b.c"));
    }

    #[test]
    fn config_bare_variable_is_boolean_true() {
        let config = GitConfig::parse(b"[core]\n\tflag\n").unwrap();
        assert_eq!(config.sections[0].entries[0].value, None);
        assert_eq!(config.get_bool("core", None, "flag"), Some(true));
        // A bare key has no string value.
        assert_eq!(config.get("core", None, "flag"), None);
    }

    #[test]
    fn config_explicit_empty_value_is_boolean_false() {
        // `x =` (with the equals) is an empty value, which git treats as false,
        // distinct from a bare key with no equals (true).
        let config = GitConfig::parse(b"[core]\n\tx =\n").unwrap();
        assert_eq!(config.sections[0].entries[0].value.as_deref(), Some(""));
        assert_eq!(config.get_bool("core", None, "x"), Some(false));
    }

    #[test]
    fn config_value_unquoted_trims_surrounding_whitespace() {
        assert_eq!(
            parse_core_x("[core]\n\tx =    hello world   \n").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn config_value_quotes_preserve_spaces() {
        assert_eq!(
            parse_core_x("[core]\n\tx = \"  spaced  \"\n").as_deref(),
            Some("  spaced  ")
        );
    }

    #[test]
    fn config_value_mixes_quoted_and_unquoted_runs() {
        assert_eq!(
            parse_core_x("[core]\n\tx = a\" b \"c\n").as_deref(),
            Some("a b c")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = \"ab\"   cd\n").as_deref(),
            Some("ab   cd")
        );
        assert_eq!(parse_core_x("[core]\n\tx = a\"\"b\n").as_deref(), Some("ab"));
    }

    #[test]
    fn config_value_processes_escapes_in_unquoted_and_quoted() {
        // Escapes are processed in both unquoted and quoted values.
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\tb\\nc\n").as_deref(),
            Some("a\tb\nc")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = \"a\\tb\"\n").as_deref(),
            Some("a\tb")
        );
        assert_eq!(parse_core_x("[core]\n\tx = a\\bb\n").as_deref(), Some("a\u{8}b"));
        assert_eq!(parse_core_x("[core]\n\tx = a\\\"b\n").as_deref(), Some("a\"b"));
        assert_eq!(parse_core_x("[core]\n\tx = a\\\\b\n").as_deref(), Some("a\\b"));
    }

    #[test]
    fn config_value_rejects_unknown_escape() {
        // \z is not a valid escape, in either quoted or unquoted values.
        assert!(GitConfig::parse(b"[core]\n\tx = a\\zb\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\tx = \"a\\zb\"\n").is_err());
    }

    #[test]
    fn config_value_line_continuation_joins_lines() {
        // A trailing backslash continues the value onto the next physical line.
        assert_eq!(
            parse_core_x("[core]\n\tx = a\\\n b\n").as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn config_value_continuation_inside_quotes() {
        // The continuation also works inside a quoted span.
        assert_eq!(
            parse_core_x("[core]\n\tx = \"a\\\n b\"\n").as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn config_value_inline_comments_stripped_outside_quotes() {
        assert_eq!(
            parse_core_x("[core]\n\tx = val ; comment\n").as_deref(),
            Some("val")
        );
        assert_eq!(
            parse_core_x("[core]\n\tx = val # comment\n").as_deref(),
            Some("val")
        );
        // Comment characters inside quotes are literal.
        assert_eq!(parse_core_x("[core]\n\tx = \"a#b\"\n").as_deref(), Some("a#b"));
        assert_eq!(
            parse_core_x("[core]\n\tx = \"ab\" ; c\n").as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn config_bare_key_with_inline_comment_is_error() {
        // git rejects a comment after a value-less key.
        assert!(GitConfig::parse(b"[core]\n\tflag ; comment\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\tflag # comment\n").is_err());
    }

    #[test]
    fn config_unterminated_quote_is_error() {
        assert!(GitConfig::parse(b"[core]\n\tx = \"ab\n").is_err());
    }

    #[test]
    fn config_trailing_backslash_at_eof_is_tolerated() {
        // A trailing backslash with no following line just ends the value.
        assert_eq!(parse_core_x("[core]\n\tx = a\\").as_deref(), Some("a"));
    }

    #[test]
    fn config_handles_crlf_line_endings() {
        assert_eq!(parse_core_x("[core]\r\n\tx = y\r\n").as_deref(), Some("y"));
    }

    #[test]
    fn config_no_spaces_around_equals() {
        assert_eq!(parse_core_x("[core]\n\tx=y\n").as_deref(), Some("y"));
    }

    #[test]
    fn config_multi_valued_keys_preserve_order_and_duplicates() {
        let config = GitConfig::parse(b"[core]\n\tx = 1\n\tx = 2\n\tx = 1\n").unwrap();
        assert_eq!(
            config.get_all("core", None, "x"),
            vec![Some("1"), Some("2"), Some("1")]
        );
        // Last value wins for the scalar getter.
        assert_eq!(config.get("core", None, "x"), Some("1"));
    }

    #[test]
    fn config_get_all_spans_multiple_sections_in_order() {
        let config =
            GitConfig::parse(b"[core]\n\tx = a\n[other]\n\ty = z\n[core]\n\tx = b\n").unwrap();
        assert_eq!(config.get_all("core", None, "x"), vec![Some("a"), Some("b")]);
    }

    #[test]
    fn config_rejects_value_before_section() {
        assert!(GitConfig::parse(b"\tx = y\n").is_err());
    }

    #[test]
    fn config_rejects_invalid_names() {
        // An underscore is not allowed in section or variable names.
        assert!(GitConfig::parse(b"[core]\n\tx_y = 1\n").is_err());
        assert!(GitConfig::parse(b"[a_b]\n\tx = 1\n").is_err());
    }

    #[test]
    fn config_variable_name_must_start_with_letter() {
        // git requires variable names to begin with an alphabetic character.
        assert!(GitConfig::parse(b"[core]\n\t1x = y\n").is_err());
        assert!(GitConfig::parse(b"[core]\n\t-x = y\n").is_err());
        // ...but a letter followed by digits/hyphens is fine.
        assert_eq!(parse_core_x("[core]\n\tx = ok\n").as_deref(), Some("ok"));
        let config = GitConfig::parse(b"[core]\n\tx1-y = z\n").unwrap();
        assert_eq!(config.get("core", None, "x1-y"), Some("z"));
    }

    #[test]
    fn config_section_name_may_start_with_digit() {
        // Unlike variable names, section names may begin with a digit.
        let config = GitConfig::parse(b"[1core]\n\tx = y\n").unwrap();
        assert_eq!(config.get("1core", None, "x"), Some("y"));
    }

    #[test]
    fn config_comments_and_blank_lines_are_skipped() {
        let config =
            GitConfig::parse(b"# top\n; also\n\n[core]\n\n\tx = y\n# trailing\n").unwrap();
        assert_eq!(config.get("core", None, "x"), Some("y"));
    }

    // ----- bool / int / bool-or-int coercion -----

    #[test]
    fn parse_config_bool_keywords() {
        for truthy in ["true", "TRUE", "yes", "Yes", "on", "ON", "1"] {
            assert_eq!(parse_config_bool(truthy), Some(true), "{truthy}");
        }
        for falsy in ["false", "FALSE", "no", "No", "off", "OFF", "0", ""] {
            assert_eq!(parse_config_bool(falsy), Some(false), "{falsy}");
        }
    }

    #[test]
    fn parse_config_bool_accepts_integers() {
        // Non-zero integers are true, zero is false (git's bool-from-int rule).
        assert_eq!(parse_config_bool("5"), Some(true));
        assert_eq!(parse_config_bool("-3"), Some(true));
        assert_eq!(parse_config_bool("0"), Some(false));
        assert_eq!(parse_config_bool("0x10"), Some(true));
        // Non-numeric, non-keyword strings are not booleans.
        assert_eq!(parse_config_bool("foo"), None);
    }

    #[test]
    fn parse_config_int_units_and_bases() {
        assert_eq!(parse_config_int("1k"), Some(1024));
        assert_eq!(parse_config_int("1K"), Some(1024));
        assert_eq!(parse_config_int("1m"), Some(1024 * 1024));
        assert_eq!(parse_config_int("1M"), Some(1024 * 1024));
        assert_eq!(parse_config_int("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_config_int("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_config_int("5"), Some(5));
        assert_eq!(parse_config_int("-5"), Some(-5));
        assert_eq!(parse_config_int("0x10"), Some(16));
        assert_eq!(parse_config_int("010"), Some(8));
    }

    #[test]
    fn parse_config_int_rejects_invalid() {
        assert_eq!(parse_config_int(""), None);
        assert_eq!(parse_config_int("foo"), None);
        assert_eq!(parse_config_int("1 k"), None);
        assert_eq!(parse_config_int("1.5"), None);
        // Overflow on the unit multiplication is rejected rather than wrapping.
        assert_eq!(parse_config_int("9999999999999999999g"), None);
    }

    #[test]
    fn parse_config_bool_or_int_typing() {
        assert_eq!(parse_config_bool_or_int("yes"), Some(ConfigBoolOrInt::Bool(true)));
        assert_eq!(
            parse_config_bool_or_int("off"),
            Some(ConfigBoolOrInt::Bool(false))
        );
        // git treats a bare empty value as false here too.
        assert_eq!(parse_config_bool_or_int(""), Some(ConfigBoolOrInt::Bool(false)));
        // Bare numbers (including 0 and 1) are integers, not booleans.
        assert_eq!(parse_config_bool_or_int("5"), Some(ConfigBoolOrInt::Int(5)));
        assert_eq!(parse_config_bool_or_int("0"), Some(ConfigBoolOrInt::Int(0)));
        assert_eq!(parse_config_bool_or_int("1"), Some(ConfigBoolOrInt::Int(1)));
        assert_eq!(parse_config_bool_or_int("1k"), Some(ConfigBoolOrInt::Int(1024)));
        assert_eq!(parse_config_bool_or_int("foo"), None);
    }

    // ----- serialization / round-trip -----

    #[test]
    fn config_canonical_value_quoting_matches_git() {
        // (value, expected serialized form of the value portion)
        let cases = [
            ("simple", "simple"),
            ("a b c", "a b c"),         // internal spaces: no quotes
            ("  lead", "\"  lead\""),   // leading space: quote
            ("trail  ", "\"trail  \""), // trailing space: quote
            ("a#b", "\"a#b\""),         // '#' forces quotes
            ("a;b", "\"a;b\""),         // ';' forces quotes
            ("a\"b", "a\\\"b"),         // embedded quote: escape, no surrounding quotes
            ("a\\b", "a\\\\b"),         // backslash escaped
            ("a\tb", "a\\tb"),          // tab escaped, no surrounding quotes
            ("a\nb", "a\\nb"),          // newline escaped
        ];
        for (value, expected) in cases {
            let config = GitConfig {
                sections: vec![ConfigSection {
                    name: "core".into(),
                    subsection: None,
                    entries: vec![ConfigEntry {
                        key: "x".into(),
                        value: Some(value.to_string()),
                    }],
                }],
            };
            let bytes = config.to_canonical_bytes();
            let text = String::from_utf8(bytes).unwrap();
            let expected_line = format!("\tx = {expected}\n");
            assert!(
                text.contains(&expected_line),
                "value {value:?} serialized to {text:?}, expected to contain {expected_line:?}"
            );
        }
    }

    #[test]
    fn config_subsection_header_only_escapes_quote_and_backslash() {
        let config = GitConfig {
            sections: vec![ConfigSection {
                name: "remote".into(),
                subsection: Some("a\"b\\c".into()),
                entries: vec![ConfigEntry {
                    key: "url".into(),
                    value: Some("x".into()),
                }],
            }],
        };
        let text = String::from_utf8(config.to_canonical_bytes()).unwrap();
        assert!(
            text.starts_with("[remote \"a\\\"b\\\\c\"]\n"),
            "unexpected header: {text:?}"
        );
    }

    #[test]
    fn config_round_trip_is_stable_for_tricky_values() {
        // parse -> serialize -> parse must be a fixpoint and preserve the value.
        let values = [
            "simple",
            "a b c",
            "  leading and trailing  ",
            "with#hash",
            "with;semi",
            "with\"quote",
            "with\\backslash",
            "with\ttab",
            "with\nnewline",
            "  # ; \" \\ \t mixed  ",
            "",
        ];
        for value in values {
            let original = GitConfig {
                sections: vec![ConfigSection {
                    name: "core".into(),
                    subsection: Some("a b\"c".into()),
                    entries: vec![
                        ConfigEntry {
                            key: "x".into(),
                            value: Some(value.to_string()),
                        },
                        // A bare boolean-true key should survive the round trip.
                        ConfigEntry {
                            key: "flag".into(),
                            value: None,
                        },
                    ],
                }],
            };
            let serialized = original.to_canonical_bytes();
            let reparsed = GitConfig::parse(&serialized).unwrap();
            assert_eq!(reparsed, original, "value {value:?} did not round-trip");
            // Serializing again must be byte-identical (stable fixpoint).
            assert_eq!(
                reparsed.to_canonical_bytes(),
                serialized,
                "value {value:?} is not a serialization fixpoint"
            );
        }
    }

    #[test]
    fn config_round_trip_preserves_multi_value_order() {
        let original = GitConfig {
            sections: vec![ConfigSection {
                name: "core".into(),
                subsection: None,
                entries: vec![
                    ConfigEntry {
                        key: "x".into(),
                        value: Some("first".into()),
                    },
                    ConfigEntry {
                        key: "x".into(),
                        value: Some("second".into()),
                    },
                    ConfigEntry {
                        key: "x".into(),
                        value: Some("first".into()),
                    },
                ],
            }],
        };
        let reparsed = GitConfig::parse(&original.to_canonical_bytes()).unwrap();
        assert_eq!(reparsed, original);
        assert_eq!(
            reparsed.get_all("core", None, "x"),
            vec![Some("first"), Some("second"), Some("first")]
        );
    }

    #[test]
    fn index_v2_round_trips_entry() {
        let index = Index {
            version: 2,
            entries: vec![IndexEntry {
                ctime_seconds: 1,
                ctime_nanoseconds: 2,
                mtime_seconds: 3,
                mtime_nanoseconds: 4,
                dev: 5,
                ino: 6,
                mode: 0o100644,
                uid: 7,
                gid: 8,
                size: 6,
                oid: ObjectId::from_hex(
                    ObjectFormat::Sha1,
                    "ce013625030ba8dba906f756967f9e9ca394464a",
                )
                .unwrap(),
                flags: 5,
                flags_extended: 0,
                path: b"a.txt".to_vec(),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_v2_sha1().unwrap();
        let parsed = Index::parse_v2_sha1(&bytes).unwrap();
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_v2_round_trips_sha256_entry() {
        let index = Index {
            version: 2,
            entries: vec![IndexEntry {
                ctime_seconds: 1,
                ctime_nanoseconds: 2,
                mtime_seconds: 3,
                mtime_nanoseconds: 4,
                dev: 5,
                ino: 6,
                mode: 0o100644,
                uid: 7,
                gid: 8,
                size: 6,
                oid: git_core::object_id_for_bytes(ObjectFormat::Sha256, "blob", b"hello\n")
                    .unwrap(),
                flags: 5,
                flags_extended: 0,
                path: b"a.txt".to_vec(),
            }],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write(ObjectFormat::Sha256).unwrap();
        let parsed = Index::parse(&bytes, ObjectFormat::Sha256).unwrap();
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
        assert!(Index::parse_v2_sha1(&bytes).is_err());
    }

    #[test]
    fn index_v4_round_trips_prefix_compressed_paths() {
        let long_path = vec![b'a'; 140];
        let index = Index {
            version: 4,
            entries: vec![
                IndexEntry {
                    ctime_seconds: 1,
                    ctime_nanoseconds: 2,
                    mtime_seconds: 3,
                    mtime_nanoseconds: 4,
                    dev: 5,
                    ino: 6,
                    mode: 0o100644,
                    uid: 7,
                    gid: 8,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "ce013625030ba8dba906f756967f9e9ca394464a",
                    )
                    .unwrap(),
                    flags: long_path.len() as u16,
                    flags_extended: 0,
                    path: long_path,
                },
                IndexEntry {
                    ctime_seconds: 9,
                    ctime_nanoseconds: 10,
                    mtime_seconds: 11,
                    mtime_nanoseconds: 12,
                    dev: 13,
                    ino: 14,
                    mode: 0o100644,
                    uid: 15,
                    gid: 16,
                    size: 1,
                    oid: ObjectId::from_hex(
                        ObjectFormat::Sha1,
                        "2e65efe2a145dda7ee51d1741299f848e5bf752e",
                    )
                    .unwrap(),
                    flags: 1,
                    flags_extended: 0,
                    path: b"b".to_vec(),
                },
            ],
            extensions: Vec::new(),
            checksum: None,
        };
        let bytes = index.write_sha1().unwrap();
        assert!(bytes.windows(3).any(|window| window == [0x80, 0x0c, b'b']));
        let parsed = Index::parse_v2_sha1(&bytes).unwrap();
        assert_eq!(parsed.version, index.version);
        assert_eq!(parsed.entries, index.entries);
        assert_eq!(parsed.extensions, index.extensions);
        assert!(parsed.checksum.is_some());
    }

    #[test]
    fn index_rejects_bad_checksum() {
        let index = Index {
            version: 2,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        };
        let mut bytes = index.write_v2_sha1().unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(Index::parse_v2_sha1(&bytes).is_err());
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(ObjectFormat::Sha1, hex).unwrap()
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("git-rs-{name}-{}-{nanos}", std::process::id()))
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
        let checksum = git_core::digest_bytes(format, &out).unwrap();
        out.extend_from_slice(checksum.as_bytes());
        out
    }

    fn commit_graph_chunks(
        entries: &[(ObjectId, ObjectId, Vec<u32>, u32, u64)],
    ) -> Vec<([u8; 4], Vec<u8>)> {
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        let object_ids: Vec<ObjectId> = entries.iter().map(|entry| entry.0.clone()).collect();
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

    /// Build a unique scratch directory under the system temp dir and create it.
    fn unique_include_dir(tag: &str) -> PathBuf {
        let dir = unique_temp_dir(tag);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn glob_matcher_handles_stars_classes_and_double_star() {
        // Single star does not cross path separators.
        assert!(glob_match("foo*", "foobar", false));
        assert!(glob_match("*bar", "foobar", false));
        assert!(!glob_match("foo*", "foo/bar", false));
        // `?` matches one non-slash char.
        assert!(glob_match("f?o", "foo", false));
        assert!(!glob_match("f?o", "f/o", false));
        // Character classes and ranges.
        assert!(glob_match("[a-c]oo", "boo", false));
        assert!(!glob_match("[a-c]oo", "zoo", false));
        assert!(glob_match("[!a-c]oo", "zoo", false));
        // `**` crosses separators, including zero directories.
        assert!(glob_match("/home/**", "/home/user/work/.git", false));
        assert!(glob_match("/home/**", "/home", false));
        assert!(glob_match("**/foo/.git", "/a/b/foo/.git", false));
        assert!(glob_match("**/foo/.git", "/foo/.git", false));
        assert!(glob_match("a/**/b", "a/b", false));
        assert!(glob_match("a/**/b", "a/x/y/b", false));
        assert!(!glob_match("a/**/b", "a/xb", false));
        // Case-insensitive matching.
        assert!(glob_match("/Home/**", "/home/user/.git", true));
        assert!(!glob_match("/Home/**", "/home/user/.git", false));
    }

    #[test]
    fn config_include_unconditional_merges_and_overrides() {
        let dir = unique_include_dir("inc-uncond");
        let main = dir.join("config");
        let extra = dir.join("extra.cfg");
        fs::write(
            &main,
            format!(
                "[core]\n\tfilemode = false\n[include]\n\tpath = {}\n",
                extra.display()
            ),
        )
        .unwrap();
        // The included file overrides filemode and adds a new value.
        fs::write(&extra, "[core]\n\tfilemode = true\n\tbig = yes\n").unwrap();

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).unwrap();
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        assert_eq!(config.get_bool("core", None, "big"), Some(true));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_relative_path_resolves_against_including_file() {
        let dir = unique_include_dir("inc-rel");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let main = dir.join("config");
        // Relative path is resolved against the including file's directory.
        fs::write(&main, "[include]\n\tpath = sub/child.cfg\n").unwrap();
        fs::write(sub.join("child.cfg"), "[user]\n\temail = a@b.c\n").unwrap();

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).unwrap();
        assert_eq!(config.get("user", None, "email"), Some("a@b.c"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_missing_file_is_ignored() {
        let dir = unique_include_dir("inc-missing");
        let main = dir.join("config");
        fs::write(
            &main,
            "[core]\n\tfilemode = true\n[include]\n\tpath = does-not-exist.cfg\n",
        )
        .unwrap();

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).unwrap();
        // No error, and the existing value is preserved.
        assert_eq!(config.get_bool("core", None, "filemode"), Some(true));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_gitdir_match_and_non_match() {
        let dir = unique_include_dir("inc-gitdir");
        let work = dir.join("work");
        fs::create_dir_all(&work).unwrap();
        let main = dir.join("config");
        let work_git = work.join(".git");
        fs::write(
            &main,
            format!(
                "[includeIf \"gitdir:{}/\"]\n\tpath = matched.cfg\n",
                work.display()
            ),
        )
        .unwrap();
        fs::write(dir.join("matched.cfg"), "[user]\n\tname = work\n").unwrap();

        // git_dir under the pattern: condition matches.
        let matching = ConfigIncludeContext::new(Some(work_git.clone()), None);
        let config = load_config_with_includes(&main, &matching).unwrap();
        assert_eq!(config.get("user", None, "name"), Some("work"));

        // git_dir elsewhere: condition does not match, nothing is spliced.
        let other = ConfigIncludeContext::new(Some(dir.join("other/.git")), None);
        let config = load_config_with_includes(&main, &other).unwrap();
        assert_eq!(config.get("user", None, "name"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_gitdir_case_insensitive() {
        let dir = unique_include_dir("inc-gitdir-i");
        let main = dir.join("config");
        fs::write(
            &main,
            "[includeIf \"gitdir/i:/SOME/Path/**\"]\n\tpath = ci.cfg\n",
        )
        .unwrap();
        fs::write(dir.join("ci.cfg"), "[user]\n\tname = ci\n").unwrap();

        let ctx = ConfigIncludeContext::new(Some(PathBuf::from("/some/path/repo/.git")), None);
        let config = load_config_with_includes(&main, &ctx).unwrap();
        assert_eq!(config.get("user", None, "name"), Some("ci"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_if_onbranch_match() {
        let dir = unique_include_dir("inc-onbranch");
        let main = dir.join("config");
        fs::write(
            &main,
            "[includeIf \"onbranch:feature/*\"]\n\tpath = feat.cfg\n",
        )
        .unwrap();
        fs::write(dir.join("feat.cfg"), "[user]\n\tname = feature\n").unwrap();

        // Matching branch.
        let on = ConfigIncludeContext::new(None, Some("feature/login".into()));
        let config = load_config_with_includes(&main, &on).unwrap();
        assert_eq!(config.get("user", None, "name"), Some("feature"));

        // Non-matching branch (slash boundary: `*` does not cross `/`).
        let off = ConfigIncludeContext::new(None, Some("main".into()));
        let config = load_config_with_includes(&main, &off).unwrap();
        assert_eq!(config.get("user", None, "name"), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_include_recursion_depth_limit() {
        let dir = unique_include_dir("inc-depth");
        // Build a chain longer than the depth limit; each file includes the next.
        let total = CONFIG_MAX_INCLUDE_DEPTH + 3;
        for i in 0..total {
            let path = dir.join(format!("c{i}.cfg"));
            let next = dir.join(format!("c{}.cfg", i + 1));
            fs::write(
                &path,
                format!("[s{i}]\n\tk = v{i}\n[include]\n\tpath = {}\n", next.display()),
            )
            .unwrap();
        }
        let entry = dir.join("c0.cfg");
        let ctx = ConfigIncludeContext::default();
        let err = load_config_with_includes(&entry, &ctx).unwrap_err();
        assert!(matches!(err, GitError::InvalidFormat(_)), "got {err:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_resolve_includes_on_parsed_value() {
        let dir = unique_include_dir("inc-parsed");
        let extra = dir.join("extra.cfg");
        fs::write(&extra, "[user]\n\temail = parsed@x.y\n").unwrap();
        let parsed = GitConfig::parse(
            format!("[include]\n\tpath = {}\n", extra.display()).as_bytes(),
        )
        .unwrap();
        // The parser leaves the include unresolved.
        assert_eq!(parsed.get("user", None, "email"), None);
        // Resolving against the base dir splices it in.
        let resolved = parsed
            .resolve_includes(&dir, &ConfigIncludeContext::default())
            .unwrap();
        assert_eq!(resolved.get("user", None, "email"), Some("parsed@x.y"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_nested_include_resolves_within_depth() {
        let dir = unique_include_dir("inc-nested");
        let main = dir.join("config");
        let mid = dir.join("mid.cfg");
        let leaf = dir.join("leaf.cfg");
        fs::write(&main, format!("[include]\n\tpath = {}\n", mid.display())).unwrap();
        fs::write(&mid, format!("[include]\n\tpath = {}\n", leaf.display())).unwrap();
        fs::write(&leaf, "[deep]\n\tvalue = ok\n").unwrap();

        let ctx = ConfigIncludeContext::default();
        let config = load_config_with_includes(&main, &ctx).unwrap();
        assert_eq!(config.get("deep", None, "value"), Some("ok"));
        fs::remove_dir_all(&dir).ok();
    }
}
