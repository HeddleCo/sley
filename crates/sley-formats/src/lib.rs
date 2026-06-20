//! git-formats — Git's remaining on-disk and wire formats: reftables, the
//! commit-graph, bundles, and repository layout.
//!
//! The object model, configuration system, and index format that used to live
//! here now have dedicated crates: [`sley_object`], [`sley_config`], and
//! [`sley_index`].

// sley#7: untrusted-input parsing crate — fallible ops propagate errors;
// the only retained `expect`s would be documented compile-time invariants.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use sley_config::{ConfigEntry, ConfigSection, GitConfig};
use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const REFTABLE_MAGIC: &[u8; 4] = b"REFT";
const REFTABLE_DEFAULT_BLOCK_SIZE: u32 = 4096;
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

/// The value of a reftable log record — either a reflog update entry or a
/// tombstone that masks an entry from an earlier table in the stack. Mirrors
/// `reftable_log_record`'s `value_type` discriminant (reftable/record.c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReftableLogValue {
    /// A log deletion (`REFTABLE_LOG_DELETION`): a placeholder that hides the
    /// `(refname, update_index)` entry recorded in an older table.
    Deletion,
    /// A reflog entry (`REFTABLE_LOG_UPDATE`).
    Update(ReftableLogUpdate),
}

/// A single reflog entry as stored in a reftable log block. The field set is
/// exactly `reftable_log_record`'s `update` arm: old/new object ids, committer
/// identity, and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReftableLogUpdate {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: u64,
    /// Committer timezone offset in `[+-]HHMM` minutes, encoded as a signed
    /// 16-bit big-endian quantity on disk.
    pub tz_offset: i16,
    pub message: String,
}

/// A reftable log record: the reflog of `refname` at a given `update_index`.
///
/// Log records are keyed by `refname` plus the *reversed* update index
/// (`~0 - update_index`), so newer entries sort first — matching
/// `reftable_log_record_key` in reftable/record.c.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReftableLogRecord {
    pub refname: String,
    pub update_index: u64,
    pub value: ReftableLogValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reftable {
    pub header: ReftableHeader,
    pub refs: Vec<ReftableRefRecord>,
    /// Reflog records carried by the table's log blocks, in on-disk order
    /// (newest `update_index` first within each refname).
    pub logs: Vec<ReftableLogRecord>,
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
        let logs = parse_reftable_log_section(bytes, &footer, footer_start, header)?;
        Ok(Self { header, refs, logs })
    }

    pub fn write_ref_only(
        format: ObjectFormat,
        min_update_index: u64,
        max_update_index: u64,
        refs: &[ReftableRefRecord],
    ) -> Result<Vec<u8>> {
        Self::write(format, min_update_index, max_update_index, refs, &[])
    }

    /// Serialize a single reftable carrying both refs and reflog records.
    ///
    /// The layout follows reftable/writer.c: a `ReftableHeader`, the `'r'` ref
    /// block(s), then the `'g'` log block(s) (zlib-deflated bodies), then the
    /// footer with `log_position` set to the start of the first log block. The
    /// reflog records are keyed by `refname` and the *reversed* update index so
    /// newer entries sort first, matching `reftable_log_record_key`.
    pub fn write(
        format: ObjectFormat,
        min_update_index: u64,
        max_update_index: u64,
        refs: &[ReftableRefRecord],
        logs: &[ReftableLogRecord],
    ) -> Result<Vec<u8>> {
        let version = match format {
            ObjectFormat::Sha1 => ReftableVersion::V1,
            ObjectFormat::Sha256 => ReftableVersion::V2,
        };
        let header = ReftableHeader {
            version,
            block_size: REFTABLE_DEFAULT_BLOCK_SIZE,
            min_update_index,
            max_update_index,
            object_format: format,
        };
        let mut refs = refs.to_vec();
        refs.sort_by(|left, right| left.name.cmp(&right.name));
        let mut out = write_reftable_header(header)?;
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
        let log_position = if logs.is_empty() {
            0
        } else {
            write_reftable_log_block(&mut out, format, logs)?
        };
        out.extend_from_slice(&write_reftable_footer(header, 0, 0, 0, 0, log_position)?);
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

fn write_reftable_header(header: ReftableHeader) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(header.version.header_len());
    out.extend_from_slice(REFTABLE_MAGIC);
    out.push(header.version.number());
    write_u24(&mut out, header.block_size)?;
    out.extend_from_slice(&header.min_update_index.to_be_bytes());
    out.extend_from_slice(&header.max_update_index.to_be_bytes());
    if header.version == ReftableVersion::V2 {
        out.extend_from_slice(match header.object_format {
            ObjectFormat::Sha1 => b"sha1",
            ObjectFormat::Sha256 => b"s256",
        });
    }
    Ok(out)
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
    let mut out = write_reftable_header(header)?;
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

/// Encode the on-disk key for a log record: `refname \0 <8-byte big-endian ts>`,
/// where `ts = ~0 - update_index` so larger update indices sort first
/// (reftable/record.c::reftable_log_record_key).
fn reftable_log_key(refname: &str, update_index: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(refname.len() + 9);
    key.extend_from_slice(refname.as_bytes());
    key.push(0);
    let ts = u64::MAX - update_index;
    key.extend_from_slice(&ts.to_be_bytes());
    key
}

/// Write the single `'g'` log block to `out`, deflating its body, and return the
/// byte offset at which the block begins (its `log_position` in the footer).
///
/// The body is built uncompressed first — `[type:1][len:3]` header, then the
/// prefix-compressed log records, the be24 restart offsets, and the be16 restart
/// count — and then everything after the 4-byte header is zlib-deflated and
/// written back over the uncompressed bytes (reftable/block.c::block_writer_finish).
fn write_reftable_log_block(
    out: &mut Vec<u8>,
    format: ObjectFormat,
    logs: &[ReftableLogRecord],
) -> Result<u64> {
    // git sorts log records by (refname asc, update_index desc) which is exactly
    // ascending key order, since the key embeds the reversed update index.
    let mut logs = logs.to_vec();
    logs.sort_by(|left, right| {
        reftable_log_key(&left.refname, left.update_index)
            .cmp(&reftable_log_key(&right.refname, right.update_index))
    });

    let block_start = out.len() as u64;
    let header_off = out.len();
    out.push(b'g');
    out.extend_from_slice(&[0, 0, 0]);

    // Build the uncompressed block body (records + restart table) in a scratch
    // buffer so we can deflate the whole thing in one shot.
    let mut body = Vec::new();
    let mut previous_key: Vec<u8> = Vec::new();
    let mut restart_offsets: Vec<u32> = Vec::new();
    // The uncompressed body's record region starts right after the 4-byte block
    // header; restart offsets are relative to the block start (header_off==0
    // here because the body buffer holds only post-header bytes, and git's
    // restart offsets point at the record start counting from the block's first
    // byte — i.e. the post-header offset plus 4).
    for (index, record) in logs.iter().enumerate() {
        let key = reftable_log_key(&record.refname, record.update_index);
        let restart = index == 0;
        let prefix_len = if restart {
            0
        } else {
            common_prefix_len(&previous_key, &key)
        };
        if restart {
            restart_offsets.push((body.len() + 4) as u32);
        }
        let val_type = match &record.value {
            ReftableLogValue::Deletion => 0u64,
            ReftableLogValue::Update(_) => 1u64,
        };
        let suffix = &key[prefix_len..];
        write_reftable_varint(&mut body, prefix_len as u64);
        write_reftable_varint(&mut body, ((suffix.len() as u64) << 3) | val_type);
        body.extend_from_slice(suffix);
        if let ReftableLogValue::Update(update) = &record.value {
            write_reftable_log_value(&mut body, format, update)?;
        }
        previous_key = key;
    }
    for offset in &restart_offsets {
        write_u24(&mut body, *offset)?;
    }
    let restart_count = u16::try_from(restart_offsets.len())
        .map_err(|_| GitError::InvalidFormat("too many reftable log restart offsets".into()))?;
    body.extend_from_slice(&restart_count.to_be_bytes());

    // The 3-byte block length records the *uncompressed* on-disk size
    // (4-byte header + uncompressed body), per block_writer_finish.
    let uncompressed_len = 4 + body.len();
    if uncompressed_len > REFTABLE_MAX_BLOCK_SIZE as usize {
        return Err(GitError::InvalidFormat(
            "reftable log block exceeds maximum size".into(),
        ));
    }
    write_u24_at(out, header_off + 1, uncompressed_len as u32)?;

    let compressed = deflate_zlib(&body)?;
    out.extend_from_slice(&compressed);
    Ok(block_start)
}

fn write_reftable_log_value(
    out: &mut Vec<u8>,
    format: ObjectFormat,
    update: &ReftableLogUpdate,
) -> Result<()> {
    if update.old_oid.format() != format || update.new_oid.format() != format {
        return Err(GitError::InvalidFormat(
            "reftable log object format mismatch".into(),
        ));
    }
    out.extend_from_slice(update.old_oid.as_bytes());
    out.extend_from_slice(update.new_oid.as_bytes());
    write_reftable_string(out, update.name.as_bytes());
    write_reftable_string(out, update.email.as_bytes());
    write_reftable_varint(out, update.time);
    out.extend_from_slice(&update.tz_offset.to_be_bytes());
    write_reftable_string(out, update.message.as_bytes());
    Ok(())
}

/// A varint-length-prefixed byte string (reftable/record.c::encode_string).
fn write_reftable_string(out: &mut Vec<u8>, bytes: &[u8]) {
    write_reftable_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn read_reftable_string(block: &[u8], offset: &mut usize, end: usize) -> Result<String> {
    let len = read_reftable_varint(block, offset, end)? as usize;
    let str_end = offset
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("reftable string overflow".into()))?;
    if str_end > end {
        return Err(GitError::InvalidFormat("truncated reftable string".into()));
    }
    let value = std::str::from_utf8(&block[*offset..str_end])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    *offset = str_end;
    Ok(value)
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// Walk the `'g'` log section starting at `footer.log_position`, inflating each
/// block and decoding its log records. Returns an empty vector when the table
/// carries no logs.
fn parse_reftable_log_section(
    bytes: &[u8],
    footer: &ReftableFooter,
    footer_start: usize,
    header: ReftableHeader,
) -> Result<Vec<ReftableLogRecord>> {
    if footer.log_position == 0 {
        return Ok(Vec::new());
    }
    let log_end = [footer.log_index_position, footer_start as u64]
        .into_iter()
        .filter(|position| *position != 0)
        .min()
        .unwrap_or(footer_start as u64) as usize;
    let mut logs = Vec::new();
    let mut offset = footer.log_position as usize;
    while offset < log_end {
        if bytes[offset] == 0 {
            offset += 1;
            continue;
        }
        if bytes[offset] != b'g' {
            break;
        }
        let uncompressed_len = read_u24(bytes, offset + 1)? as usize;
        // The on-disk block body after the 4-byte header is zlib-compressed and
        // runs to the next block (or the log index / footer). We inflate the
        // remaining bytes of the section; zlib stops at the stream end.
        let compressed = &bytes[offset + 4..log_end];
        let (inflated, consumed) = inflate_zlib(compressed, uncompressed_len.saturating_sub(4))?;
        // Reconstruct the logical block: 4-byte header + inflated body.
        let mut block = Vec::with_capacity(uncompressed_len);
        block.extend_from_slice(&bytes[offset..offset + 4]);
        block.extend_from_slice(&inflated);
        logs.extend(parse_reftable_log_block(&block, header)?);
        offset += 4 + consumed;
    }
    Ok(logs)
}

fn parse_reftable_log_block(
    block: &[u8],
    header: ReftableHeader,
) -> Result<Vec<ReftableLogRecord>> {
    if block.len() < 6 || block[0] != b'g' {
        return Err(GitError::InvalidFormat("invalid reftable log block".into()));
    }
    let restart_count = read_u16(block, block.len() - 2)? as usize;
    if restart_count == 0 {
        return Err(GitError::InvalidFormat(
            "reftable log block has no restart offsets".into(),
        ));
    }
    let restart_table_start = block
        .len()
        .checked_sub(2 + restart_count * 3)
        .ok_or_else(|| GitError::InvalidFormat("truncated reftable log restart table".into()))?;
    let mut offset = 4;
    let mut previous_key: Vec<u8> = Vec::new();
    let mut records = Vec::new();
    while offset < restart_table_start {
        let record = parse_reftable_log_record(
            block,
            &mut offset,
            restart_table_start,
            header,
            &mut previous_key,
        )?;
        records.push(record);
    }
    if offset != restart_table_start {
        return Err(GitError::InvalidFormat(
            "reftable log block ended inside record".into(),
        ));
    }
    Ok(records)
}

fn parse_reftable_log_record(
    block: &[u8],
    offset: &mut usize,
    end: usize,
    header: ReftableHeader,
    previous_key: &mut Vec<u8>,
) -> Result<ReftableLogRecord> {
    let prefix_len = read_reftable_varint(block, offset, end)? as usize;
    if prefix_len > previous_key.len() {
        return Err(GitError::InvalidFormat(
            "reftable log prefix exceeds previous key".into(),
        ));
    }
    let suffix_len_and_type = read_reftable_varint(block, offset, end)?;
    let suffix_len = (suffix_len_and_type >> 3) as usize;
    let value_type = (suffix_len_and_type & 0x7) as u8;
    let suffix_end = offset
        .checked_add(suffix_len)
        .ok_or_else(|| GitError::InvalidFormat("reftable log suffix overflow".into()))?;
    if suffix_end > end {
        return Err(GitError::InvalidFormat(
            "truncated reftable log suffix".into(),
        ));
    }
    let mut key = previous_key[..prefix_len].to_vec();
    key.extend_from_slice(&block[*offset..suffix_end]);
    *offset = suffix_end;
    // The key is `refname \0 <8-byte BE ts>`: at least 9 trailing bytes, and the
    // byte before the timestamp must be the NUL separator.
    if key.len() < 9 || key[key.len() - 9] != 0 {
        return Err(GitError::InvalidFormat("malformed reftable log key".into()));
    }
    let refname = std::str::from_utf8(&key[..key.len() - 9])
        .map_err(|err| GitError::InvalidFormat(err.to_string()))?
        .to_string();
    let ts_bytes: [u8; 8] = key[key.len() - 8..]
        .try_into()
        .map_err(|_| GitError::InvalidFormat("truncated reftable log timestamp".into()))?;
    let update_index = u64::MAX - u64::from_be_bytes(ts_bytes);
    *previous_key = key;
    let value = match value_type {
        0 => ReftableLogValue::Deletion,
        1 => {
            let old_oid = read_reftable_oid(block, offset, end, header.object_format)?;
            let new_oid = read_reftable_oid(block, offset, end, header.object_format)?;
            let name = read_reftable_string(block, offset, end)?;
            let email = read_reftable_string(block, offset, end)?;
            let time = read_reftable_varint(block, offset, end)?;
            let tz_offset = i16::from_be_bytes([
                *block
                    .get(*offset)
                    .ok_or_else(|| GitError::InvalidFormat("truncated reftable log tz".into()))?,
                *block.get(*offset + 1).ok_or_else(|| {
                    GitError::InvalidFormat("truncated reftable log tz".into())
                })?,
            ]);
            *offset += 2;
            let message = read_reftable_string(block, offset, end)?;
            ReftableLogValue::Update(ReftableLogUpdate {
                old_oid,
                new_oid,
                name,
                email,
                time,
                tz_offset,
                message,
            })
        }
        other => {
            return Err(GitError::InvalidFormat(format!(
                "unsupported reftable log value type {other}"
            )));
        }
    };
    Ok(ReftableLogRecord {
        refname,
        update_index,
        value,
    })
}

/// Deflate `data` with a zlib header/trailer at level 9, matching git's
/// `deflateInit(stream, 9)` for reftable log blocks.
fn deflate_zlib(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(9));
    encoder
        .write_all(data)
        .map_err(|err| GitError::InvalidFormat(format!("reftable log deflate failed: {err}")))?;
    encoder
        .finish()
        .map_err(|err| GitError::InvalidFormat(format!("reftable log deflate failed: {err}")))
}

/// Inflate a single zlib stream from `data`, expecting `expected_len`
/// decompressed bytes. Returns the decompressed body and the number of
/// compressed bytes consumed, so the caller can find the next log block.
fn inflate_zlib(data: &[u8], expected_len: usize) -> Result<(Vec<u8>, usize)> {
    use flate2::Decompress;
    use flate2::FlushDecompress;
    let mut decoder = Decompress::new(true);
    let mut out = Vec::with_capacity(expected_len);
    decoder
        .decompress_vec(data, &mut out, FlushDecompress::Finish)
        .map_err(|err| GitError::InvalidFormat(format!("reftable log inflate failed: {err}")))?;
    Ok((out, decoder.total_in() as usize))
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

impl CommitGraphEntry {
    /// Borrowed parent positions into the containing commit-graph's sorted commit
    /// table.
    pub fn parent_indices(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        self.parents.iter().copied()
    }

    pub fn first_parent_index(&self) -> Option<u32> {
        self.parents.first().copied()
    }
}

/// Borrowed iterator over a commit-graph entry's parent object ids.
pub struct CommitGraphParentOids<'a> {
    graph: &'a CommitGraph,
    parents: std::slice::Iter<'a, u32>,
}

impl Iterator for CommitGraphParentOids<'_> {
    type Item = ObjectId;

    fn next(&mut self) -> Option<Self::Item> {
        let parent = *self.parents.next()?;
        let idx = usize::try_from(parent).ok()?;
        self.graph.commits.get(idx).map(|entry| entry.oid)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.parents.size_hint()
    }
}

impl ExactSizeIterator for CommitGraphParentOids<'_> {}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitGraphBloomSettings {
    pub hash_version: u32,
    pub hash_count: u32,
    pub bits_per_entry: u32,
    pub max_changed_paths: usize,
}

pub const DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS: CommitGraphBloomSettings =
    CommitGraphBloomSettings {
        hash_version: 1,
        hash_count: 7,
        bits_per_entry: 10,
        max_changed_paths: 512,
    };

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitGraphWriteEntry {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub generation: u32,
    pub commit_time: u64,
    pub bloom_filter: Option<Vec<u8>>,
}

impl CommitGraph {
    pub fn write(format: ObjectFormat, entries: &[CommitGraphWriteEntry]) -> Result<Vec<u8>> {
        Self::write_with_bloom_settings(format, entries, DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS)
    }

    pub fn write_with_bloom_settings(
        format: ObjectFormat,
        entries: &[CommitGraphWriteEntry],
        bloom_settings: CommitGraphBloomSettings,
    ) -> Result<Vec<u8>> {
        // Default generation version is 2 ⇒ write the GDA2 corrected-commit-date
        // chunk (matches git's `commitGraph.generationVersion` default).
        Self::write_with_options(format, entries, bloom_settings, true)
    }

    /// Write a commit-graph, optionally including the GDA2/GDO2 generation-data
    /// chunks. `write_generation_data == false` mirrors
    /// `commitGraph.generationVersion=1`, in which git stores only the
    /// topological level (in CDAT) and omits the corrected-commit-date chunks.
    pub fn write_with_options(
        format: ObjectFormat,
        entries: &[CommitGraphWriteEntry],
        bloom_settings: CommitGraphBloomSettings,
        write_generation_data: bool,
    ) -> Result<Vec<u8>> {
        validate_commit_graph_bloom_settings(bloom_settings)?;
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        validate_commit_graph_write_entries(format, &entries)?;
        let object_ids = entries.iter().map(|entry| entry.oid).collect::<Vec<_>>();
        let (cdat, edge) = write_commit_graph_commit_data(&entries)?;
        let mut chunks = vec![
            (*b"OIDF", write_commit_graph_fanout(&object_ids)?),
            (*b"OIDL", write_commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if write_generation_data {
            chunks.push((*b"GDA2", write_commit_graph_generation_data(&entries)?));
            let gdo2 = write_commit_graph_generation_overflow(&entries)?;
            if !gdo2.is_empty() {
                chunks.push((*b"GDO2", gdo2));
            }
        }
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        let bloom = write_commit_graph_bloom_filters(&entries, bloom_settings);
        if let Some((bidx, bdat)) = bloom {
            chunks.push((*b"BIDX", bidx));
            chunks.push((*b"BDAT", bdat));
        }
        write_commit_graph_chunks(format, 0, &chunks)
    }

    /// Write an incremental commit-graph layer.
    ///
    /// `base_graphs` are the chain layer ids, base first, and `base_oids` are
    /// the commit ids from those base layers in graph-position order. Parent
    /// positions in this layer are encoded against `base_oids + entries`, which
    /// matches Git's split commit-graph addressing model.
    pub fn write_with_base_options(
        format: ObjectFormat,
        entries: &[CommitGraphWriteEntry],
        bloom_settings: CommitGraphBloomSettings,
        write_generation_data: bool,
        base_graphs: &[ObjectId],
        base_oids: &[ObjectId],
    ) -> Result<Vec<u8>> {
        validate_commit_graph_bloom_settings(bloom_settings)?;
        if base_graphs.len() > u8::MAX as usize {
            return Err(GitError::InvalidFormat(
                "too many commit-graph base layers".into(),
            ));
        }
        for oid in base_graphs.iter().chain(base_oids.iter()) {
            if oid.format() != format {
                return Err(GitError::InvalidObjectId(
                    "commit-graph base format does not match graph format".into(),
                ));
            }
        }
        let mut entries = entries.to_vec();
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        validate_commit_graph_write_entries(format, &entries)?;
        let object_ids = entries.iter().map(|entry| entry.oid).collect::<Vec<_>>();
        let parent_positions = commit_graph_parent_positions(&entries, base_oids)?;
        let (cdat, edge) = write_commit_graph_commit_data_with_positions(&entries, &parent_positions)?;
        let mut chunks = vec![
            (*b"OIDF", write_commit_graph_fanout(&object_ids)?),
            (*b"OIDL", write_commit_graph_oid_lookup(&object_ids)),
            (*b"CDAT", cdat),
        ];
        if write_generation_data {
            if base_oids.is_empty() {
                chunks.push((*b"GDA2", write_commit_graph_generation_data(&entries)?));
                let gdo2 = write_commit_graph_generation_overflow(&entries)?;
                if !gdo2.is_empty() {
                    chunks.push((*b"GDO2", gdo2));
                }
            } else {
                chunks.push((*b"GDA2", vec![0; entries.len() * 4]));
            }
        }
        if !edge.is_empty() {
            chunks.push((*b"EDGE", edge));
        }
        let bloom = write_commit_graph_bloom_filters(&entries, bloom_settings);
        if let Some((bidx, bdat)) = bloom {
            chunks.push((*b"BIDX", bidx));
            chunks.push((*b"BDAT", bdat));
        }
        if !base_graphs.is_empty() {
            let mut base = Vec::with_capacity(base_graphs.len() * format.raw_len());
            for oid in base_graphs {
                base.extend_from_slice(oid.as_bytes());
            }
            chunks.push((*b"BASE", base));
        }
        write_commit_graph_chunks(format, base_graphs.len() as u8, &chunks)
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
        let mut commits =
            parse_commit_graph_commit_data(bytes, &chunks, format, oids, base_graph_count)?;
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

    /// Borrow parent object ids for `entry` without allocating a `Vec`.
    ///
    /// The parser validates parent indices while reading `CDAT`/`EDGE`, but this
    /// method rechecks them so callers using manually constructed graphs get a
    /// structured error instead of a truncated iterator.
    pub fn parent_oids<'a>(
        &'a self,
        entry: &'a CommitGraphEntry,
    ) -> Result<CommitGraphParentOids<'a>> {
        for parent in entry.parent_indices() {
            let idx = usize::try_from(parent).map_err(|_| {
                GitError::InvalidFormat("commit-graph parent index overflow".into())
            })?;
            if idx >= self.commits.len() {
                return Err(GitError::InvalidFormat(
                    "commit-graph parent points past commit table".into(),
                ));
            }
        }
        Ok(CommitGraphParentOids {
            graph: self,
            parents: entry.parents.iter(),
        })
    }
}

impl CommitGraphBloomFilters {
    pub fn filter_for_commit(&self, commit_index: usize) -> Option<&[u8]> {
        self.filters.get(commit_index).map(Vec::as_slice)
    }

    pub fn contains_path(&self, commit_index: usize, path: &[u8]) -> Option<bool> {
        let filter = self.filter_for_commit(commit_index)?;
        Some(commit_graph_bloom_filter_contains(
            filter,
            path,
            CommitGraphBloomSettings {
                hash_version: self.hash_version,
                hash_count: self.hash_count,
                bits_per_entry: self.bits_per_entry,
                max_changed_paths: DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS.max_changed_paths,
            },
        ))
    }
}

fn validate_commit_graph_bloom_settings(settings: CommitGraphBloomSettings) -> Result<()> {
    match settings.hash_version {
        1 | 2 => Ok(()),
        other => Err(GitError::Unsupported(format!(
            "commit-graph changed-path Bloom filter hash version {other}"
        ))),
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
    let parent_positions = commit_graph_parent_positions(entries, &[])?;
    write_commit_graph_commit_data_with_positions(entries, &parent_positions)
}

fn commit_graph_parent_positions(
    entries: &[CommitGraphWriteEntry],
    base_oids: &[ObjectId],
) -> Result<HashMap<ObjectId, u32>> {
    let mut positions = HashMap::with_capacity(base_oids.len() + entries.len());
    for (idx, oid) in base_oids.iter().enumerate() {
        let idx = u32::try_from(idx)
            .map_err(|_| GitError::InvalidFormat("commit-graph base position overflow".into()))?;
        positions.entry(*oid).or_insert(idx);
    }
    let base_len = u32::try_from(base_oids.len())
        .map_err(|_| GitError::InvalidFormat("commit-graph base position overflow".into()))?;
    for (idx, entry) in entries.iter().enumerate() {
        let idx = u32::try_from(idx)
            .map_err(|_| GitError::InvalidFormat("commit-graph position overflow".into()))?;
        let pos = base_len
            .checked_add(idx)
            .ok_or_else(|| GitError::InvalidFormat("commit-graph position overflow".into()))?;
        if positions.insert(entry.oid, pos).is_some() {
            return Err(GitError::InvalidFormat(
                "commit-graph contains duplicate object ids".into(),
            ));
        }
    }
    Ok(positions)
}

fn write_commit_graph_commit_data_with_positions(
    entries: &[CommitGraphWriteEntry],
    parent_positions_by_oid: &HashMap<ObjectId, u32>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut cdat = Vec::new();
    let mut edge = Vec::new();
    for entry in entries {
        cdat.extend_from_slice(entry.tree.as_bytes());
        let parent_positions = entry
            .parents
            .iter()
            .map(|parent| {
                parent_positions_by_oid.get(parent).copied().ok_or_else(|| {
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

fn write_commit_graph_generation_data(entries: &[CommitGraphWriteEntry]) -> Result<Vec<u8>> {
    let corrected_dates = corrected_commit_dates(entries)?;
    let mut overflow_index = 0u32;
    let mut out = Vec::with_capacity(entries.len() * 4);
    for (entry, corrected) in entries.iter().zip(corrected_dates) {
        let offset = corrected.saturating_sub(entry.commit_time);
        if offset > COMMIT_GRAPH_GENERATION_DATA_MAX {
            out.extend_from_slice(
                &(COMMIT_GRAPH_GENERATION_DATA_OVERFLOW | overflow_index).to_be_bytes(),
            );
            overflow_index = overflow_index.checked_add(1).ok_or_else(|| {
                GitError::InvalidFormat("commit-graph GDO2 chunk overflow".into())
            })?;
        } else {
            out.extend_from_slice(&(offset as u32).to_be_bytes());
        }
    }
    Ok(out)
}

fn write_commit_graph_generation_overflow(entries: &[CommitGraphWriteEntry]) -> Result<Vec<u8>> {
    let corrected_dates = corrected_commit_dates(entries)?;
    let mut out = Vec::new();
    for (entry, corrected) in entries.iter().zip(corrected_dates) {
        let offset = corrected.saturating_sub(entry.commit_time);
        if offset > COMMIT_GRAPH_GENERATION_DATA_MAX {
            out.extend_from_slice(&offset.to_be_bytes());
        }
    }
    Ok(out)
}

fn corrected_commit_dates(entries: &[CommitGraphWriteEntry]) -> Result<Vec<u64>> {
    fn corrected_at(
        idx: usize,
        entries: &[CommitGraphWriteEntry],
        cache: &mut [Option<u64>],
    ) -> Result<u64> {
        if let Some(value) = cache[idx] {
            return Ok(value);
        }
        let entry = &entries[idx];
        let mut corrected = entry.commit_time;
        for parent in &entry.parents {
            let parent_idx = entries
                .binary_search_by(|entry| entry.oid.as_bytes().cmp(parent.as_bytes()))
                .map_err(|_| {
                    GitError::InvalidFormat(format!(
                        "commit-graph parent {parent} is missing from graph"
                    ))
                })?;
            corrected = corrected.max(corrected_at(parent_idx, entries, cache)?.saturating_add(1));
        }
        cache[idx] = Some(corrected);
        Ok(corrected)
    }

    let mut cache = vec![None; entries.len()];
    for idx in 0..entries.len() {
        corrected_at(idx, entries, &mut cache)?;
    }
    cache
        .into_iter()
        .map(|value| {
            value.ok_or_else(|| {
                GitError::InvalidFormat("commit-graph corrected date missing".into())
            })
        })
        .collect()
}

fn write_commit_graph_bloom_filters(
    entries: &[CommitGraphWriteEntry],
    settings: CommitGraphBloomSettings,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if entries.iter().all(|entry| entry.bloom_filter.is_none()) {
        return None;
    }
    let mut bidx = Vec::with_capacity(entries.len() * 4);
    let mut bdat = Vec::new();
    bdat.extend_from_slice(&settings.hash_version.to_be_bytes());
    bdat.extend_from_slice(&settings.hash_count.to_be_bytes());
    bdat.extend_from_slice(&settings.bits_per_entry.to_be_bytes());
    let mut offset = 0u32;
    for entry in entries {
        if let Some(filter) = &entry.bloom_filter {
            offset = offset.checked_add(filter.len() as u32)?;
            bdat.extend_from_slice(filter);
        }
        bidx.extend_from_slice(&offset.to_be_bytes());
    }
    Some((bidx, bdat))
}

pub fn commit_graph_bloom_filter_for_paths<I, P>(
    paths: I,
    settings: CommitGraphBloomSettings,
) -> Vec<u8>
where
    I: IntoIterator<Item = P>,
    P: AsRef<[u8]>,
{
    let mut unique = std::collections::BTreeSet::new();
    for path in paths {
        let path = path.as_ref();
        if path.is_empty() {
            continue;
        }
        unique.insert(path.to_vec());
        for idx in path
            .iter()
            .enumerate()
            .filter_map(|(idx, byte)| (*byte == b'/').then_some(idx))
        {
            if idx > 0 {
                unique.insert(path[..idx].to_vec());
            }
        }
    }
    if unique.len() > settings.max_changed_paths {
        return vec![0xff];
    }
    let filter_len = (unique.len() as u64 * u64::from(settings.bits_per_entry)).div_ceil(8);
    let mut filter = vec![0u8; usize::try_from(filter_len).unwrap_or(usize::MAX).max(1)];
    for path in unique {
        add_commit_graph_bloom_key(&mut filter, &path, settings);
    }
    filter
}

pub fn commit_graph_bloom_filter_contains(
    filter: &[u8],
    path: &[u8],
    settings: CommitGraphBloomSettings,
) -> bool {
    if filter.is_empty() {
        return false;
    }
    commit_graph_bloom_key_hashes(path, settings)
        .into_iter()
        .all(|hash| {
            let bit = u64::from(hash) % ((filter.len() as u64) * 8);
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit & 7);
            filter.get(byte).is_some_and(|value| value & mask != 0)
        })
}

fn add_commit_graph_bloom_key(filter: &mut [u8], path: &[u8], settings: CommitGraphBloomSettings) {
    if filter.is_empty() {
        return;
    }
    for hash in commit_graph_bloom_key_hashes(path, settings) {
        let bit = u64::from(hash) % ((filter.len() as u64) * 8);
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit & 7);
        if let Some(value) = filter.get_mut(byte) {
            *value |= mask;
        }
    }
}

fn commit_graph_bloom_key_hashes(path: &[u8], settings: CommitGraphBloomSettings) -> Vec<u32> {
    let seed0 = 0x293a_e76f;
    let seed1 = 0x7e64_6e2c;
    let hash0 = commit_graph_bloom_murmur3_seeded(seed0, path, settings.hash_version);
    let hash1 = commit_graph_bloom_murmur3_seeded(seed1, path, settings.hash_version);
    (0..settings.hash_count)
        .map(|idx| hash0.wrapping_add(idx.wrapping_mul(hash1)))
        .collect()
}

fn commit_graph_bloom_murmur3_seeded(seed: u32, data: &[u8], version: u32) -> u32 {
    let c1 = 0xcc9e_2d51u32;
    let c2 = 0x1b87_3593u32;
    let r1 = 15;
    let r2 = 13;
    let m = 5u32;
    let n = 0xe654_6b64u32;
    let mut seed = seed;
    for chunk in data.chunks_exact(4) {
        let mut k = if version == 2 {
            u32::from(chunk[0])
                | (u32::from(chunk[1]) << 8)
                | (u32::from(chunk[2]) << 16)
                | (u32::from(chunk[3]) << 24)
        } else {
            (chunk[0] as i8 as i32 as u32)
                | ((chunk[1] as i8 as i32 as u32) << 8)
                | ((chunk[2] as i8 as i32 as u32) << 16)
                | ((chunk[3] as i8 as i32 as u32) << 24)
        };
        k = k.wrapping_mul(c1);
        k = k.rotate_left(r1);
        k = k.wrapping_mul(c2);
        seed ^= k;
        seed = seed.rotate_left(r2).wrapping_mul(m).wrapping_add(n);
    }

    let tail = data.chunks_exact(4).remainder();
    let mut k1 = 0u32;
    let tail_byte = |idx: usize| -> u32 {
        if version == 2 {
            u32::from(tail[idx])
        } else {
            tail[idx] as i8 as i32 as u32
        }
    };
    match tail.len() {
        3 => {
            k1 ^= tail_byte(2) << 16;
            k1 ^= tail_byte(1) << 8;
            k1 ^= tail_byte(0);
        }
        2 => {
            k1 ^= tail_byte(1) << 8;
            k1 ^= tail_byte(0);
        }
        1 => {
            k1 ^= tail_byte(0);
        }
        _ => {}
    }
    if !tail.is_empty() {
        k1 = k1.wrapping_mul(c1);
        k1 = k1.rotate_left(r1);
        k1 = k1.wrapping_mul(c2);
        seed ^= k1;
    }

    seed ^= data.len() as u32;
    seed ^= seed >> 16;
    seed = seed.wrapping_mul(0x85eb_ca6b);
    seed ^= seed >> 13;
    seed = seed.wrapping_mul(0xc2b2_ae35);
    seed ^ (seed >> 16)
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
const COMMIT_GRAPH_GENERATION_DATA_OVERFLOW: u32 = 0x8000_0000;
const COMMIT_GRAPH_GENERATION_DATA_MAX: u64 = 0x7fff_ffff;

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
    base_graph_count: u8,
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
    let parent_limit = if base_graph_count == 0 {
        commit_count
    } else {
        usize::MAX
    };
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
        let parents = commit_graph_parents(parent_one, parent_two, extra_edges, parent_limit)?;
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
    parent_limit: usize,
) -> Result<Vec<u32>> {
    let mut parents = Vec::new();
    if parent_one != COMMIT_GRAPH_PARENT_NONE {
        validate_commit_graph_parent_position(parent_one, parent_limit)?;
        parents.push(parent_one);
    }
    if parent_two == COMMIT_GRAPH_PARENT_NONE {
        return Ok(parents);
    }
    if parent_two & COMMIT_GRAPH_EXTRA_EDGE == 0 {
        validate_commit_graph_parent_position(parent_two, parent_limit)?;
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
        validate_commit_graph_parent_position(parent, parent_limit)?;
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

/// How refs are stored in a newly initialized repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefStorageFormat {
    Files,
    Reftable,
}

impl RefStorageFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "" | "files" => Ok(Self::Files),
            "reftable" => Ok(Self::Reftable),
            other => Err(GitError::Command(format!(
                "unknown ref storage format '{other}'"
            ))),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Reftable => "reftable",
        }
    }
}

/// Fully-resolved options for [`RepositoryBootstrap::init`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    pub worktree: PathBuf,
    /// Explicit git directory (e.g. from `GIT_DIR` during `git init`). When set,
    /// the repository's git directory lives here instead of `<worktree>/.git`,
    /// and *no* `gitdir:` link file is written into the worktree — exactly like
    /// `GIT_DIR=<dir> git init` (init-db.c leaves the worktree untouched).
    pub git_dir_override: Option<PathBuf>,
    /// Value to record as `core.worktree` in the new repository's config
    /// (init-db.c writes it when the git directory is not `<worktree>/.git`).
    pub core_worktree: Option<String>,
    pub object_format: ObjectFormat,
    /// Whether `object_format` came from an explicit `--object-format` flag (as
    /// opposed to an env/config default). On reinit, an explicit format that differs
    /// from the existing repository is a fatal error; a defaulted one is ignored.
    pub object_format_explicit: bool,
    pub bare: bool,
    pub initial_branch: String,
    pub template_dir: Option<PathBuf>,
    pub copy_template_config: bool,
    pub separate_git_dir: Option<PathBuf>,
    pub shared_repository: Option<String>,
    pub ref_storage: RefStorageFormat,
    /// Whether `ref_storage` came from an explicit `--ref-format` flag (as opposed to
    /// an env/config default). On reinit, an explicit format that differs from the
    /// existing repository is a fatal error; a defaulted one is ignored.
    pub ref_storage_explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
    pub git_dir: PathBuf,
    pub worktree: PathBuf,
    pub object_format: ObjectFormat,
    pub bare: bool,
    pub ref_storage: RefStorageFormat,
    pub reinitialized: bool,
}

pub struct RepositoryBootstrap;

struct ReferenceBackendEnv {
    raw: String,
    format: RefStorageFormat,
    path: PathBuf,
}

fn reference_backend_env() -> Result<Option<ReferenceBackendEnv>> {
    let Ok(raw) = env::var("GIT_REFERENCE_BACKEND") else {
        return Ok(None);
    };
    let Some((backend, path)) = raw.split_once("://") else {
        return Err(GitError::Command(format!(
            "invalid value for 'extensions.refstorage': '{raw}'"
        )));
    };
    if path.is_empty() {
        return Err(GitError::Command(format!(
            "invalid value for 'extensions.refstorage': '{raw}'"
        )));
    }
    let format = match backend {
        "files" => RefStorageFormat::Files,
        "reftable" => RefStorageFormat::Reftable,
        _ => {
            return Err(GitError::Command(format!(
                "invalid value for 'extensions.refstorage': '{raw}'"
            )));
        }
    };
    let path = PathBuf::from(path);
    Ok(Some(ReferenceBackendEnv {
        raw,
        format,
        path,
    }))
}

impl RepositoryBootstrap {
    pub fn init(options: InitOptions) -> Result<RepositoryLayout> {
        if options.bare && options.separate_git_dir.is_some() {
            return Err(GitError::Command(
                "options '--bare' and '--separate-git-dir' cannot be used together".into(),
            ));
        }

        let worktree = options.worktree;
        if !options.bare {
            fs::create_dir_all(&worktree)?;
        }

        let dot_git = worktree.join(".git");
        // git computes `original_git_dir = real_pathdup(git_dir)` and writes the
        // `gitdir:` link there. When `.git` is a symlink to the real git directory,
        // git therefore moves the *resolved* directory and rewrites the gitlink
        // through the symlink (so `.git` stays a symlink and its target becomes the
        // gitlink file). `git_link` is the path that receives the `gitdir:` line.
        let dot_git_is_symlink = fs::symlink_metadata(&dot_git)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        let git_link = if dot_git_is_symlink {
            fs::canonicalize(&dot_git).unwrap_or_else(|_| dot_git.clone())
        } else {
            dot_git.clone()
        };
        let mut write_git_link = false;
        let git_dir = if let Some(git_dir_override) = options.git_dir_override.clone() {
            git_dir_override
        } else if options.bare {
            worktree.clone()
        } else if git_link.is_file() {
            let existing = read_gitdir_file(&git_link)?.ok_or_else(|| {
                GitError::InvalidFormat(format!("invalid gitfile {}", git_link.display()))
            })?;
            if let Some(new_separate) = options.separate_git_dir.clone() {
                if existing != new_separate {
                    if existing.exists() && !new_separate.exists() {
                        if let Some(parent) = new_separate.parent() {
                            create_shared_dir(parent, options.shared_repository.as_deref())?;
                        }
                        fs::rename(&existing, &new_separate)?;
                        // git's `separate_git_dir()` calls
                        // `repair_worktrees_after_gitdir_move()` so the linked
                        // worktrees keep resolving to the relocated git dir.
                        repair_worktrees_after_gitdir_move(&existing, &new_separate)?;
                    }
                    write_git_link = true;
                    new_separate
                } else {
                    existing
                }
            } else {
                existing
            }
        } else if let Some(separate_git_dir) = options.separate_git_dir.clone() {
            write_git_link = true;
            if let Some(parent) = separate_git_dir.parent() {
                create_shared_dir(parent, options.shared_repository.as_deref())?;
            }
            // Move the *resolved* git directory (`git_link`), which is `.git` itself
            // when it is a real directory or its symlink target when `.git` is a
            // symlink. `is_dir()` follows symlinks, so it is true in both cases.
            if git_link.is_dir() && !separate_git_dir.exists() {
                fs::rename(&git_link, &separate_git_dir)?;
                // After moving the common git dir, repoint every linked
                // worktree's `.git`/`gitdir` link pair (init-db.c →
                // `separate_git_dir` → `repair_worktrees_after_gitdir_move`).
                repair_worktrees_after_gitdir_move(&git_link, &separate_git_dir)?;
            }
            separate_git_dir
        } else {
            worktree.join(".git")
        };

        let reinitialized = git_dir.join("HEAD").exists();

        // On reinit the existing repository's formats are authoritative. Git refuses an
        // explicit `--object-format`/`--ref-format` that disagrees with the existing
        // repository (it could corrupt the object store), but silently ignores env/config
        // defaults that disagree. A defaulted format never rewrites an existing repo, so
        // the effective formats below drive both the on-disk layout and the returned
        // `RepositoryLayout`.
        let alt_ref_storage = if reinitialized {
            None
        } else {
            reference_backend_env()?
        };
        let (object_format, ref_storage) = if reinitialized {
            let existing = read_existing_repository_formats(&git_dir)?;
            if options.object_format_explicit && options.object_format != existing.0 {
                return Err(GitError::Command(
                    "attempt to reinitialize repository with different hash".into(),
                ));
            }
            if options.ref_storage_explicit && options.ref_storage != existing.1 {
                return Err(GitError::Command(
                    "attempt to reinitialize repository with different reference storage format"
                        .into(),
                ));
            }
            existing
        } else {
            (
                options.object_format,
                alt_ref_storage
                    .as_ref()
                    .map(|backend| backend.format)
                    .unwrap_or(options.ref_storage),
            )
        };
        let ref_storage_dir = alt_ref_storage
            .as_ref()
            .map(|backend| backend.path.clone())
            .unwrap_or_else(|| git_dir.clone());

        create_shared_dir(&git_dir, options.shared_repository.as_deref())?;
        create_shared_dir(
            git_dir.join("objects/info"),
            options.shared_repository.as_deref(),
        )?;
        create_shared_dir(
            git_dir.join("objects/pack"),
            options.shared_repository.as_deref(),
        )?;

        if ref_storage == RefStorageFormat::Files {
            create_shared_dir(
                ref_storage_dir.join("refs/heads"),
                options.shared_repository.as_deref(),
            )?;
            create_shared_dir(
                ref_storage_dir.join("refs/tags"),
                options.shared_repository.as_deref(),
            )?;
        } else {
            create_shared_dir(
                ref_storage_dir.join("reftable"),
                options.shared_repository.as_deref(),
            )?;
        }

        let head_path = git_dir.join("HEAD");
        if let Some(backend) = alt_ref_storage.as_ref() {
            fs::create_dir_all(git_dir.join("refs"))?;
            fs::write(git_dir.join("refs").join("heads"), b"repository uses alternate refs storage\n")?;
            if !head_path.exists() {
                fs::write(&head_path, b"ref: refs/heads/.invalid\n")?;
            }
            if ref_storage == RefStorageFormat::Files {
                fs::write(
                    ref_storage_dir.join("HEAD"),
                    format!("ref: refs/heads/{}\n", options.initial_branch),
                )?;
            } else {
                write_initial_reftable(
                    &ref_storage_dir,
                    object_format,
                    &options.initial_branch,
                    options.shared_repository.as_deref(),
                )?;
            }
            let _ = backend;
        } else if ref_storage == RefStorageFormat::Files {
            if !head_path.exists() {
                fs::write(
                    &head_path,
                    format!("ref: refs/heads/{}\n", options.initial_branch),
                )?;
            }
        } else if !head_path.exists() {
            fs::write(&head_path, b"ref: refs/heads/.invalid\n")?;
            write_initial_reftable(
                &ref_storage_dir,
                object_format,
                &options.initial_branch,
                options.shared_repository.as_deref(),
            )?;
        }

        let config_path = git_dir.join("config");
        if !config_path.exists() {
            fs::write(
                config_path,
                build_init_config(
                    object_format,
                    options.bare,
                    &options.shared_repository,
                    ref_storage,
                    alt_ref_storage.as_ref().map(|backend| backend.raw.as_str()),
                    options.core_worktree.as_deref(),
                )
                .to_canonical_bytes(),
            )?;
        }

        if let Some(template_dir) = options.template_dir.as_deref() {
            apply_init_template(
                template_dir,
                &git_dir,
                options.copy_template_config,
                options.shared_repository.as_deref(),
            )?;
        }

        if write_git_link {
            let link_target = gitdir_link_path(&worktree, &git_dir)?;
            // Write through `git_link`: identical to `.git` in the common case, but
            // the symlink *target* when `.git` is a symlink (git leaves the symlink
            // itself intact and turns its target into the gitlink file).
            fs::write(&git_link, format!("gitdir: {link_target}\n"))?;
            apply_shared_file_mode(&git_link, options.shared_repository.as_deref())?;
        }

        Ok(RepositoryLayout {
            git_dir,
            worktree,
            object_format,
            bare: options.bare,
            ref_storage,
            reinitialized,
        })
    }
}

/// Read the object and ref storage formats recorded in an existing repository's config.
///
/// `git_dir` must be the repository's (common) git directory. A missing or unreadable
/// config — or an absent `extensions.*` key — falls back to git's defaults (sha1 /
/// files), matching how git treats a freshly created v0 repository.
fn read_existing_repository_formats(git_dir: &Path) -> Result<(ObjectFormat, RefStorageFormat)> {
    let Ok(config) = GitConfig::read(git_dir.join("config")) else {
        return Ok((ObjectFormat::Sha1, RefStorageFormat::Files));
    };
    let object_format = config.repository_object_format()?;
    let ref_storage = match config.get("extensions", None, "refStorage") {
        Some(value) if value.eq_ignore_ascii_case("reftable") => RefStorageFormat::Reftable,
        _ => RefStorageFormat::Files,
    };
    Ok((object_format, ref_storage))
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
        RepositoryBootstrap::init(InitOptions {
            git_dir_override: None,
            core_worktree: None,
            worktree: path.as_ref().to_path_buf(),
            object_format,
            object_format_explicit: false,
            bare,
            initial_branch: initial_branch.into(),
            template_dir: None,
            copy_template_config: false,
            separate_git_dir: None,
            shared_repository: None,
            ref_storage: RefStorageFormat::Files,
            ref_storage_explicit: false,
        })
    }
}

fn build_init_config(
    object_format: ObjectFormat,
    bare: bool,
    shared_repository: &Option<String>,
    ref_storage: RefStorageFormat,
    ref_storage_value: Option<&str>,
    core_worktree: Option<&str>,
) -> GitConfig {
    let uses_extensions =
        object_format == ObjectFormat::Sha256
            || ref_storage == RefStorageFormat::Reftable
            || ref_storage_value.is_some();
    let mut core_entries = vec![
        ConfigEntry::new(
            "repositoryformatversion",
            Some(if uses_extensions { "1" } else { "0" }.into()),
        ),
        ConfigEntry::new("filemode", Some("true".into())),
        ConfigEntry::new("bare", Some(if bare { "true" } else { "false" }.into())),
    ];
    if !bare {
        // init-db.c `create_default_files`: a non-bare init records
        // `core.logallrefupdates = true` (unless a template config already
        // chose a value, which `apply_init_template` honours by overriding).
        core_entries.push(ConfigEntry::new("logallrefupdates", Some("true".into())));
        if let Some(worktree) = core_worktree {
            core_entries.push(ConfigEntry::new("worktree", Some(worktree.into())));
        }
    }
    if let Some(shared) = shared_repository
        && let Some(value) = shared_repository_config_value(shared)
    {
        core_entries.push(ConfigEntry::new("sharedRepository", Some(value)));
    }
    let mut sections = vec![ConfigSection::new("core", None, core_entries)];
    let mut extension_entries = Vec::new();
    if object_format == ObjectFormat::Sha256 {
        extension_entries.push(ConfigEntry::new("objectformat", Some("sha256".into())));
    }
    if let Some(value) = ref_storage_value {
        extension_entries.push(ConfigEntry::new("refStorage", Some(value.into())));
    } else if ref_storage == RefStorageFormat::Reftable {
        extension_entries.push(ConfigEntry::new("refStorage", Some("reftable".into())));
    }
    if !extension_entries.is_empty() {
        sections.push(ConfigSection::new("extensions", None, extension_entries));
    }
    GitConfig {
        preamble: Vec::new(),
        suffix: Vec::new(),
        sections,
    }
}

fn write_initial_reftable(
    git_dir: &Path,
    object_format: ObjectFormat,
    initial_branch: &str,
    shared_repository: Option<&str>,
) -> Result<()> {
    let update_index = 1;
    let table_name = format!("0x{update_index:012x}-0x{update_index:012x}-00000000.ref");
    let refs = vec![ReftableRefRecord {
        name: "HEAD".into(),
        update_index,
        value: ReftableRefValue::Symbolic(format!("refs/heads/{initial_branch}")),
    }];
    let bytes = Reftable::write_ref_only(object_format, update_index, update_index, &refs)?;
    let reftable_dir = git_dir.join("reftable");
    let table_path = reftable_dir.join(&table_name);
    fs::write(&table_path, bytes)?;
    apply_shared_file_mode(&table_path, shared_repository)?;
    let list_path = reftable_dir.join("tables.list");
    fs::write(&list_path, format!("{table_name}\n"))?;
    apply_shared_file_mode(&list_path, shared_repository)?;
    let refs_dir = git_dir.join("refs");
    fs::create_dir_all(&refs_dir)?;
    let refs_heads = refs_dir.join("heads");
    fs::write(
        &refs_heads,
        b"this repository uses the reftable format\n" as &[u8],
    )?;
    apply_shared_file_mode(&refs_heads, shared_repository)?;
    Ok(())
}

fn apply_init_template(
    template_dir: &Path,
    git_dir: &Path,
    copy_config: bool,
    shared_repository: Option<&str>,
) -> Result<()> {
    if !template_dir.is_dir() {
        return Ok(());
    }
    copy_init_template_entries(template_dir, git_dir, shared_repository)?;
    let template_config_path = template_dir.join("config");
    if copy_config && template_config_path.is_file() {
        let mut template_config = GitConfig::read(template_config_path)?;
        let current_config = GitConfig::read(git_dir.join("config"))?;
        template_config.sections.extend(current_config.sections);
        fs::write(git_dir.join("config"), template_config.to_canonical_bytes())?;
    }
    Ok(())
}

fn copy_init_template_entries(
    source: &Path,
    destination: &Path,
    shared_repository: Option<&str>,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        if source_path.is_dir() {
            if !destination_path.exists() {
                create_shared_dir(&destination_path, shared_repository)?;
            }
            copy_init_template_entries(&source_path, &destination_path, shared_repository)?;
        } else if name != "config" && !destination_path.exists() {
            if let Some(parent) = destination_path.parent() {
                create_shared_dir(parent, shared_repository)?;
            }
            fs::copy(source_path, &destination_path)?;
            apply_shared_file_mode(&destination_path, shared_repository)?;
        }
    }
    Ok(())
}

/// Port of git's `repair_worktrees_after_gitdir_move()` (worktree.c). After the
/// common git directory is relocated from `old_git_dir` to `new_git_dir`
/// (`git init --separate-git-dir` on a repository that owns linked worktrees),
/// every linked worktree's `.git` gitfile and its admin `gitdir` backlink must
/// be rewritten so the two keep pointing at each other through the new location.
fn repair_worktrees_after_gitdir_move(old_git_dir: &Path, new_git_dir: &Path) -> Result<()> {
    let worktrees_dir = new_git_dir.join("worktrees");
    let entries = match fs::read_dir(&worktrees_dir) {
        Ok(entries) => entries,
        // No linked worktrees: nothing to repair (matches git skipping the loop).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let admin_dir = entry.path();
        if !admin_dir.is_dir() {
            continue;
        }
        repair_worktree_after_gitdir_move(&admin_dir, old_git_dir)?;
    }
    Ok(())
}

/// Port of git's `repair_worktree_after_gitdir_move()`. `admin_dir` is the
/// already-relocated `<new_git_dir>/worktrees/<id>` directory; `old_git_dir` is
/// the pre-move common git dir, used to resolve a *relative* backlink that was
/// written against the old location.
fn repair_worktree_after_gitdir_move(admin_dir: &Path, old_git_dir: &Path) -> Result<()> {
    let gitdir_file = admin_dir.join("gitdir");
    let Ok(raw) = fs::read_to_string(&gitdir_file) else {
        return Ok(());
    };
    let backlink = raw.trim();
    if backlink.is_empty() {
        return Ok(());
    }
    let backlink_path = PathBuf::from(backlink);
    let is_relative = backlink_path.is_relative();
    // Resolve the worktree's `.git` path. git stores the backlink either
    // absolute or relative to the worktree's *old* admin dir
    // (`<old_git_dir>/worktrees/<id>/`).
    let id = admin_dir.file_name().unwrap_or_default();
    let dotgit_abs = if is_relative {
        let old_admin = old_git_dir.join("worktrees").join(id);
        normalize_lexical(&old_admin.join(&backlink_path))
    } else {
        normalize_lexical(&backlink_path)
    };
    if !dotgit_abs.exists() {
        return Ok(());
    }
    write_worktree_linking_files(&dotgit_abs, &gitdir_file, is_relative)?;
    Ok(())
}

/// Port of git's `write_worktree_linking_files()`. `dotgit` is the worktree's
/// `.git` gitfile (absolute); `gitdir_file` is the admin `gitdir` backlink. When
/// `use_relative` both links are written relative to one another, otherwise both
/// are absolute. The two paths used by git are the worktree root (`dotgit`
/// minus the trailing `/.git`) and the admin dir (`gitdir_file` minus the
/// trailing `/gitdir`), each `realpath`-resolved.
fn write_worktree_linking_files(
    dotgit: &Path,
    gitdir_file: &Path,
    use_relative: bool,
) -> Result<()> {
    let worktree_root = dotgit.parent().unwrap_or(Path::new("."));
    let worktree_root =
        fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf());
    let admin_dir = gitdir_file.parent().unwrap_or(Path::new("."));
    let admin_dir = fs::canonicalize(admin_dir).unwrap_or_else(|_| admin_dir.to_path_buf());
    let worktree_dotgit = worktree_root.join(".git");
    if use_relative {
        // gitdir backlink: path to the worktree's `.git`, relative to the admin dir.
        let backlink = relative_path_lexical(&worktree_dotgit, &admin_dir);
        fs::write(gitdir_file, format!("{backlink}\n"))?;
        // worktree `.git`: path to the admin dir, relative to the worktree root.
        let link = relative_path_lexical(&admin_dir, &worktree_root);
        fs::write(&worktree_dotgit, format!("gitdir: {link}\n"))?;
    } else {
        fs::write(gitdir_file, format!("{}\n", worktree_dotgit.display()))?;
        fs::write(
            &worktree_dotgit,
            format!("gitdir: {}\n", admin_dir.display()),
        )?;
    }
    Ok(())
}

/// Lexically normalize a path (collapse `.`/`..`, no filesystem access), so a
/// path built from a relative backlink against a non-existent (already-moved)
/// old admin dir still resolves. Mirrors the cleanup git's
/// `strbuf_realpath_forgiving` performs on the components that do exist.
fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Compute `target` expressed relative to `base`, both absolute. Mirrors git's
/// `relative_path()` for the common ancestor case (the only one that arises for
/// sibling worktree/admin directories under a shared parent).
fn relative_path_lexical(target: &Path, base: &Path) -> String {
    let target = normalize_lexical(target);
    let base = normalize_lexical(base);
    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();
    let common = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..base_components.len() {
        result.push("..");
    }
    for component in &target_components[common..] {
        result.push(component.as_os_str());
    }
    if result.as_os_str().is_empty() {
        ".".to_string()
    } else {
        result.display().to_string()
    }
}

fn read_gitdir_file(path: &Path) -> Result<Option<PathBuf>> {
    let contents = fs::read_to_string(path)?;
    let Some(target) = contents.trim().strip_prefix("gitdir:") else {
        return Ok(None);
    };
    let target = PathBuf::from(target.trim());
    if target.is_absolute() {
        Ok(Some(target))
    } else {
        Ok(Some(
            path.parent().unwrap_or_else(|| Path::new("")).join(target),
        ))
    }
}

/// Compute the value git writes into the worktree's `.git` link file.
///
/// git's `separate_git_dir()` writes `gitdir: <real_git_dir>`, where `real_git_dir`
/// was made absolute via `real_pathdup()` (resolving symlinks) up front in
/// `init-db.c`. So the link always holds an *absolute, symlink-resolved* path — never
/// a relative one. We mirror that by canonicalizing the now-created git directory.
fn gitdir_link_path(_worktree: &Path, git_dir: &Path) -> Result<String> {
    let resolved = fs::canonicalize(git_dir).unwrap_or_else(|_| git_dir.to_path_buf());
    Ok(resolved.display().to_string())
}

fn create_shared_dir(path: impl AsRef<Path>, shared_repository: Option<&str>) -> Result<()> {
    let path = path.as_ref();
    if path.exists() {
        apply_shared_dir_mode(path, shared_repository)?;
        return Ok(());
    }
    fs::create_dir_all(path)?;
    apply_shared_dir_mode(path, shared_repository)?;
    Ok(())
}

fn apply_shared_dir_mode(path: &Path, shared_repository: Option<&str>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Some(perm) = shared_repository.and_then(parse_shared_repository_perm) else {
            return Ok(());
        };
        let metadata = fs::metadata(path)?;
        if metadata.is_dir() {
            let old_mode = metadata.permissions().mode();
            let new_mode = calc_shared_perm(perm, old_mode, true);
            if new_mode & 0o7777 != old_mode & 0o7777 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(new_mode & 0o7777);
                fs::set_permissions(path, permissions)?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, shared_repository);
    }
    Ok(())
}

fn apply_shared_file_mode(path: &Path, shared_repository: Option<&str>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Some(perm) = shared_repository.and_then(parse_shared_repository_perm) else {
            return Ok(());
        };
        let metadata = fs::metadata(path)?;
        if metadata.is_file() {
            let old_mode = metadata.permissions().mode();
            let new_mode = calc_shared_perm(perm, old_mode, false);
            if new_mode & 0o7777 != old_mode & 0o7777 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(new_mode & 0o7777);
                fs::set_permissions(path, permissions)?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, shared_repository);
    }
    Ok(())
}

/// Canonical `core.sharedRepository` value written at init, matching git's
/// numeric compatibility encoding (`group` → `1`, `all` → `2`, octal filemodes
/// verbatim).
fn shared_repository_config_value(shared_repository: &str) -> Option<String> {
    match shared_repository {
        "false" | "0" | "umask" => None,
        "group" | "1" | "true" => Some("1".into()),
        "all" | "world" | "everybody" | "2" | "3" => Some("2".into()),
        value if value.starts_with('0') && value.len() > 1 => Some(value.to_string()),
        _ => None,
    }
}

/// A parsed `core.sharedRepository` permission request, mirroring git's
/// `git_config_perm` (setup.c): keywords map to the group/everybody OR-in
/// tweaks, while an octal filemode (other than the 0/1/2 compatibility values)
/// *replaces* the permission bits outright.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharedPerm {
    /// `PERM_GROUP` (0660): OR group read/write into the existing mode.
    Group,
    /// `PERM_EVERYBODY` (0664): OR group rw + world read into the existing mode.
    Everybody,
    /// A literal octal filemode (`-(mode & 0666)` in git): replace the bits.
    Exact(u32),
}

#[cfg(unix)]
fn parse_shared_repository_perm(value: &str) -> Option<SharedPerm> {
    match value {
        "umask" | "false" | "no" | "off" => None,
        "group" | "true" | "yes" | "on" => Some(SharedPerm::Group),
        "all" | "world" | "everybody" => Some(SharedPerm::Everybody),
        _ => {
            let parsed = u32::from_str_radix(value, 8).ok()?;
            match parsed {
                0 => None,
                1 => Some(SharedPerm::Group),
                2 => Some(SharedPerm::Everybody),
                // git dies when the owner would lose read/write; we skip the
                // adjustment instead (init has already validated the value).
                mode if (mode & 0o600) == 0o600 => Some(SharedPerm::Exact(mode & 0o666)),
                _ => None,
            }
        }
    }
}

/// Port of git's `calc_shared_perm` + the directory tweaks from
/// `adjust_shared_perm` (path.c): never grant group/other write to a file the
/// owner cannot write, copy read bits to execute bits for executables and
/// directories, and set-gid directories that grant any group access.
#[cfg(unix)]
fn calc_shared_perm(perm: SharedPerm, mode: u32, is_dir: bool) -> u32 {
    let (base, exact) = match perm {
        SharedPerm::Group => (0o660, false),
        SharedPerm::Everybody => (0o664, false),
        SharedPerm::Exact(bits) => (bits, true),
    };
    let mut tweak = base;
    if mode & 0o200 == 0 {
        tweak &= !0o222;
    }
    if mode & 0o100 != 0 {
        tweak |= (tweak & 0o444) >> 2;
    }
    let mut new_mode = if exact {
        (mode & !0o777) | tweak
    } else {
        mode | tweak
    };
    if is_dir {
        new_mode |= (new_mode & 0o444) >> 2;
        if new_mode & 0o060 != 0 {
            new_mode |= 0o2000;
        }
    }
    new_mode
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
            let table_name = "0x000000000001-0x000000000001-00000000.ref";
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
    fn reftable_log_records_round_trip() {
        let old = oid("0000000000000000000000000000000000000000");
        let new1 = oid("1111111111111111111111111111111111111111");
        let new2 = oid("2222222222222222222222222222222222222222");
        let refs = vec![ReftableRefRecord {
            name: "refs/heads/main".into(),
            update_index: 2,
            value: ReftableRefValue::Direct(new2.clone()),
        }];
        let logs = vec![
            ReftableLogRecord {
                refname: "refs/heads/main".into(),
                update_index: 1,
                value: ReftableLogValue::Update(ReftableLogUpdate {
                    old_oid: old.clone(),
                    new_oid: new1.clone(),
                    name: "Committer One".into(),
                    email: "one@example.com".into(),
                    time: 1_700_000_000,
                    tz_offset: 120,
                    message: "commit (initial): first".into(),
                }),
            },
            ReftableLogRecord {
                refname: "refs/heads/main".into(),
                update_index: 2,
                value: ReftableLogValue::Update(ReftableLogUpdate {
                    old_oid: new1.clone(),
                    new_oid: new2.clone(),
                    name: "Committer Two".into(),
                    email: "two@example.com".into(),
                    time: 1_700_000_100,
                    tz_offset: -300,
                    message: "commit: second".into(),
                }),
            },
        ];

        let bytes = Reftable::write(ObjectFormat::Sha1, 1, 2, &refs, &logs)
            .expect("test operation should succeed");
        let table = Reftable::parse(&bytes).expect("test operation should succeed");

        assert_eq!(table.refs.len(), 1);
        // Newest update index sorts first within a refname.
        assert_eq!(table.logs.len(), 2);
        assert_eq!(table.logs[0].update_index, 2);
        assert_eq!(table.logs[1].update_index, 1);
        let ReftableLogValue::Update(update0) = &table.logs[0].value else {
            panic!("expected update");
        };
        assert_eq!(update0.tz_offset, -300);
        assert_eq!(update0.old_oid, new1);
        assert_eq!(update0.new_oid, new2);
        assert_eq!(update0.message, "commit: second");
        let ReftableLogValue::Update(update1) = &table.logs[1].value else {
            panic!("expected update");
        };
        assert_eq!(update1.tz_offset, 120);
        assert_eq!(update1.old_oid, old);
        assert_eq!(update1.message, "commit (initial): first");
    }

    /// Anti-stub conformance: upstream git must read the reflog out of a reftable
    /// our writer produced, log blocks and all.
    #[test]
    fn upstream_git_reads_rust_written_reftable_log() {
        let root = unique_temp_dir("reftable-log-upstream");
        fs::create_dir_all(&root).expect("create temp repo");
        {
            run_success("git", &root, &["init", "-q"]);
            // Build a real commit so the ref points at a commit (git's reflog show
            // requires it). Empty tree -> commit, both written into the loose ODB.
            let empty_tree = run_success_with_stdin("git", &root, &["mktree"], b"");
            let empty_tree = String::from_utf8(empty_tree).expect("tree oid is utf8");
            let commit_oid = run_success(
                "git",
                &root,
                &[
                    "-c",
                    "user.name=Reftable Writer",
                    "-c",
                    "user.email=writer@example.com",
                    "commit-tree",
                    empty_tree.trim(),
                    "-m",
                    "seed",
                ],
            );
            let commit_str = String::from_utf8(commit_oid).expect("commit oid is utf8");
            let new_oid = ObjectId::from_hex(ObjectFormat::Sha1, commit_str.trim())
                .expect("test operation should succeed");
            let zero = oid("0000000000000000000000000000000000000000");
            let git_dir = root.join(".git");
            fs::write(
                git_dir.join("config"),
                b"[core]\n\trepositoryformatversion = 1\n[extensions]\n\trefStorage = reftable\n",
            )
            .expect("write config");
            fs::write(git_dir.join("HEAD"), b"ref: refs/heads/.invalid\n").expect("write HEAD");
            let reftable_dir = git_dir.join("reftable");
            fs::create_dir_all(&reftable_dir).expect("create reftable dir");
            // git's `table_has_valid_name` parses all three dash-separated fields
            // as hex; the trailing token must be pure hex, ending in `.ref`.
            let table_name = "0x000000000001-0x000000000001-00000001.ref";
            let table = Reftable::write(
                ObjectFormat::Sha1,
                1,
                1,
                &[ReftableRefRecord {
                    name: "refs/heads/main".into(),
                    update_index: 1,
                    value: ReftableRefValue::Direct(new_oid.clone()),
                }],
                &[ReftableLogRecord {
                    refname: "refs/heads/main".into(),
                    update_index: 1,
                    value: ReftableLogValue::Update(ReftableLogUpdate {
                        old_oid: zero,
                        new_oid: new_oid.clone(),
                        name: "Reftable Writer".into(),
                        email: "writer@example.com".into(),
                        time: 1_700_000_000,
                        tz_offset: 0,
                        // git stores reflog messages with a trailing newline.
                        message: "commit (initial): seed\n".into(),
                    }),
                }],
            )
            .expect("test operation should succeed");
            fs::write(reftable_dir.join(table_name), table).expect("write reftable");
            fs::write(reftable_dir.join("tables.list"), format!("{table_name}\n"))
                .expect("write tables.list");

            // git parses the log block and surfaces the reflog message.
            let output = run_success(
                "git",
                &root,
                &["reflog", "show", "--format=%H %gs", "refs/heads/main"],
            );
            let text = String::from_utf8(output).expect("reflog output is utf8");
            assert!(
                text.contains("commit (initial): seed"),
                "git reflog did not surface our log message: {text:?}"
            );
            assert!(
                text.contains(&new_oid.to_string()),
                "git reflog did not surface our new oid: {text:?}"
            );
        };
        if std::env::var_os("SLEY_KEEP").is_none() {
            let _ = fs::remove_dir_all(&root);
        }
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
                    bloom_filter: None,
                },
                CommitGraphWriteEntry {
                    oid: base,
                    tree,
                    parents: Vec::new(),
                    generation: 1,
                    commit_time: 10,
                    bloom_filter: None,
                },
                CommitGraphWriteEntry {
                    oid: main.clone(),
                    tree,
                    parents: vec![base],
                    generation: 2,
                    commit_time: 20,
                    bloom_filter: None,
                },
                CommitGraphWriteEntry {
                    oid: side.clone(),
                    tree,
                    parents: vec![base],
                    generation: 2,
                    commit_time: 21,
                    bloom_filter: None,
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
    fn writes_commit_graph_bloom_filters_with_requested_hash_version() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut settings = DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
        settings.hash_version = 2;
        let bytes = CommitGraph::write_with_bloom_settings(
            ObjectFormat::Sha1,
            &[CommitGraphWriteEntry {
                oid: oid("1111111111111111111111111111111111111111"),
                tree,
                parents: Vec::new(),
                generation: 1,
                commit_time: 1,
                bloom_filter: Some(vec![0xaa, 0xbb]),
            }],
            settings,
        )
        .expect("test operation should succeed");

        let parsed =
            CommitGraph::parse(&bytes, ObjectFormat::Sha1).expect("test operation should succeed");
        let bloom = parsed
            .bloom_filters
            .expect("test operation should parse bloom filters");
        assert_eq!(bloom.hash_version, 2);
        assert_eq!(bloom.hash_count, settings.hash_count);
        assert_eq!(bloom.bits_per_entry, settings.bits_per_entry);
        assert_eq!(bloom.filters, vec![vec![0xaa, 0xbb]]);
    }

    #[test]
    fn rejects_unsupported_commit_graph_bloom_hash_version_on_write() {
        let tree = oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut settings = DEFAULT_COMMIT_GRAPH_BLOOM_SETTINGS;
        settings.hash_version = 3;

        assert!(
            CommitGraph::write_with_bloom_settings(
                ObjectFormat::Sha1,
                &[CommitGraphWriteEntry {
                    oid: oid("1111111111111111111111111111111111111111"),
                    tree,
                    parents: Vec::new(),
                    generation: 1,
                    commit_time: 1,
                    bloom_filter: Some(vec![0xaa]),
                }],
                settings,
            )
            .is_err()
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

    #[test]
    fn repository_bootstrap_copies_template_files() {
        let root = unique_temp_dir("init-template");
        let template = root.join("template");
        let repo = root.join("repo");
        fs::create_dir_all(template.join("hooks")).expect("create template hooks");
        fs::write(template.join("description"), b"template repo\n").expect("write description");
        fs::write(template.join("hooks/pre-commit"), b"#!/bin/sh\n").expect("write hook");
        fs::write(template.join("config"), b"[user]\n\tname = Template User\n")
            .expect("write template config");

        let layout = RepositoryBootstrap::init(InitOptions {
            git_dir_override: None,
            core_worktree: None,
            worktree: repo.clone(),
            object_format: ObjectFormat::Sha1,
            object_format_explicit: false,
            bare: false,
            initial_branch: "main".into(),
            template_dir: Some(template),
            copy_template_config: true,
            separate_git_dir: None,
            shared_repository: None,
            ref_storage: RefStorageFormat::Files,
            ref_storage_explicit: false,
        })
        .expect("init should succeed");

        assert_eq!(
            fs::read_to_string(layout.git_dir.join("description")).expect("read description"),
            "template repo\n"
        );
        assert!(layout.git_dir.join("hooks/pre-commit").is_file());
        let config = GitConfig::read(layout.git_dir.join("config")).expect("read config");
        assert_eq!(config.get("user", None, "name"), Some("Template User"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repository_bootstrap_separate_git_dir_writes_gitfile() {
        let root = unique_temp_dir("init-separate-gitdir");
        let worktree = root.join("worktree");
        let gitdir = root.join("external.git");
        let layout = RepositoryBootstrap::init(InitOptions {
            git_dir_override: None,
            core_worktree: None,
            worktree: worktree.clone(),
            object_format: ObjectFormat::Sha1,
            object_format_explicit: false,
            bare: false,
            initial_branch: "main".into(),
            template_dir: None,
            copy_template_config: false,
            separate_git_dir: Some(gitdir.clone()),
            shared_repository: None,
            ref_storage: RefStorageFormat::Files,
            ref_storage_explicit: false,
        })
        .expect("init should succeed");

        assert_eq!(layout.git_dir, gitdir);
        assert!(gitdir.join("HEAD").is_file());
        let gitfile = fs::read_to_string(worktree.join(".git")).expect("read gitfile");
        assert!(gitfile.starts_with("gitdir: "));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repository_bootstrap_reftable_writes_extension_and_table() {
        let root = unique_temp_dir("init-reftable");
        let repo = root.join("repo");
        let layout = RepositoryBootstrap::init(InitOptions {
            git_dir_override: None,
            core_worktree: None,
            worktree: repo,
            object_format: ObjectFormat::Sha1,
            object_format_explicit: false,
            bare: false,
            initial_branch: "main".into(),
            template_dir: None,
            copy_template_config: false,
            separate_git_dir: None,
            shared_repository: None,
            ref_storage: RefStorageFormat::Reftable,
            ref_storage_explicit: false,
        })
        .expect("init should succeed");

        let config = GitConfig::read(layout.git_dir.join("config")).expect("read config");
        assert_eq!(
            config.get("extensions", None, "refStorage"),
            Some("reftable")
        );
        assert!(layout.git_dir.join("reftable/tables.list").is_file());
        assert_eq!(
            fs::read(layout.git_dir.join("refs/heads")).expect("read reftable sentinel"),
            b"this repository uses the reftable format\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repository_bootstrap_honors_shared_repository_config() {
        let root = unique_temp_dir("init-shared");
        let repo = root.join("repo");
        let layout = RepositoryBootstrap::init(InitOptions {
            git_dir_override: None,
            core_worktree: None,
            worktree: repo,
            object_format: ObjectFormat::Sha1,
            object_format_explicit: false,
            bare: false,
            initial_branch: "main".into(),
            template_dir: None,
            copy_template_config: false,
            separate_git_dir: None,
            shared_repository: Some("group".into()),
            ref_storage: RefStorageFormat::Files,
            ref_storage_explicit: false,
        })
        .expect("init should succeed");

        let config = GitConfig::read(layout.git_dir.join("config")).expect("read config");
        assert_eq!(config.get("core", None, "sharedRepository"), Some("1"));
        let _ = fs::remove_dir_all(&root);
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
