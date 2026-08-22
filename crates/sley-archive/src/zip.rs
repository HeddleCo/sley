//! `git archive --format=zip` serializer.
//!
//! Byte-compatible with upstream `archive-zip.c` for the common
//! (single-disk, non-zip64) case: a stream of local-file headers + data,
//! followed by the central directory and end-of-central-directory record.
//! Each entry carries an extended-timestamp extra field (`0x5455`, mtime
//! only) exactly as git does; the central-directory entry repeats it.
//!
//! The serialization mirrors git field-for-field: local header magic
//! `0x04034b50`, central magic `0x02014b50`, trailer magic `0x06054b50`,
//! DOS date/time derived from the commit time via [`dos_time`], external
//! attributes encoding the unix mode in the high 16 bits, and the
//! deflate-or-store fallback (store when deflate does not shrink the blob).

use crate::{ArchiveConvert, ArchiveEntry, ArchiveExtras, ArchiveSink, write_archive_entries};
use flate2::{Compress, Compression, FlushCompress, Status};
use sley_core::{ObjectFormat, ObjectId, Result};
use sley_odb::ObjectReader;
use std::io::Write;

const ZIP_METHOD_STORE: u16 = 0;
const ZIP_METHOD_DEFLATE: u16 = 8;
const ZIP_UTF8: u16 = 1 << 11;

/// Extended-timestamp extra field: magic `0x5455` ("UT"), 5-byte payload
/// (`flags` + 4-byte little-endian mtime), flag bit 1 = "mtime present".
const ZIP_EXTRA_MTIME_SIZE: u16 = 9;
const ZIP_EXTRA_MTIME_PAYLOAD_SIZE: u16 = 5;
const ZIP_MAX_16: u64 = 0xffff;
const ZIP_MAX_32: u64 = 0xffff_ffff;
const ZIP64_END_OF_CENTRAL_DIRECTORY_RECORD_SIZE: u64 = 44;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZipArchiveOptions {
    pub prefix: Vec<u8>,
    pub strip_prefix: Vec<u8>,
    /// Unix time embedded as each entry's mtime, and (via DOS conversion) the
    /// zip date/time fields. Upstream uses the commit time, or the current
    /// time for a bare tree-ish.
    pub mtime: u64,
    /// When set, the hex commit id is appended as the end-of-central-directory
    /// comment (matching `git get-tar-commit-id`'s zip counterpart). `None`
    /// for tree-ish archives.
    pub commit_id: Option<ObjectId>,
    pub pathspecs: Vec<Vec<u8>>,
    /// zlib compression level (0 = store, 1..=9 = deflate). Upstream default is
    /// 6 (zlib default); `-0` forces store.
    pub compression_level: u32,
}

/// Serialize `tree_oid` as a zip archive (no content conversion).
pub fn write_zip_archive<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: ZipArchiveOptions,
) -> Result<()>
where
    R: ObjectReader,
    W: Write,
{
    write_zip_archive_inner(
        writer,
        reader,
        format,
        tree_oid,
        options,
        None,
        &ArchiveExtras::default(),
    )
}

/// Serialize `tree_oid` as a zip archive, applying smudge conversion (EOL +
/// `filter.<name>.smudge`) to each regular-file blob, matching `git archive`.
pub fn write_zip_archive_with_convert<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: ZipArchiveOptions,
    convert: &ArchiveConvert<'_>,
) -> Result<()>
where
    R: ObjectReader,
    W: Write,
{
    write_zip_archive_inner(
        writer,
        reader,
        format,
        tree_oid,
        options,
        Some(convert),
        &ArchiveExtras::default(),
    )
}

/// Like [`write_zip_archive_with_convert`] but also appends `--add-file` /
/// `--add-virtual-file` entries after the tree, before the central directory.
pub fn write_zip_archive_full<W>(
    writer: &mut W,
    reader: &sley_odb::FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: ZipArchiveOptions,
    convert: &ArchiveConvert<'_>,
    extra: &ArchiveExtras,
) -> Result<()>
where
    W: Write + ?Sized,
{
    write_zip_archive_inner(
        writer,
        reader,
        format,
        tree_oid,
        options,
        Some(convert),
        extra,
    )
}

fn write_zip_archive_inner<R, W>(
    writer: &mut W,
    reader: &R,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    options: ZipArchiveOptions,
    convert: Option<&ArchiveConvert<'_>>,
    extra: &ArchiveExtras,
) -> Result<()>
where
    R: ObjectReader,
    W: Write + ?Sized,
{
    // Validate pathspecs before writing any output, matching git's
    // `parse_pathspec_arg`: an unmatched pathspec must `die()` with no archive
    // bytes on the stream.
    crate::validate_archive_pathspecs(reader, format, tree_oid, &options.pathspecs, convert)?;
    let prefix = crate::normalize_prefix(&options.prefix)?;
    let strip_prefix = crate::normalize_strip_prefix(&options.strip_prefix)?;
    let (zip_date, zip_time) = dos_time(options.mtime);
    let mut sink = ZipSink {
        writer,
        central_dir: Vec::new(),
        offset: 0,
        entries: 0,
        max_creator_version: 0,
        mtime: options.mtime as u32,
        zip_date,
        zip_time,
        compression_level: options.compression_level,
    };
    write_archive_entries(
        &mut sink,
        reader,
        format,
        tree_oid,
        &prefix,
        &strip_prefix,
        &options.pathspecs,
        convert,
    )?;
    extra.emit_into(&mut sink, format)?;
    sink.write_trailer(options.commit_id.as_ref(), format)?;
    Ok(())
}

struct ZipSink<'a, W: Write + ?Sized> {
    writer: &'a mut W,
    /// Accumulated central-directory bytes, flushed after all entries.
    central_dir: Vec<u8>,
    /// Running offset of the local-header stream (start of the next entry).
    offset: u64,
    entries: u64,
    max_creator_version: u16,
    mtime: u32,
    zip_date: u16,
    zip_time: u16,
    compression_level: u32,
}

impl<W: Write + ?Sized> ArchiveSink for ZipSink<'_, W> {
    fn emit(&mut self, entry: ArchiveEntry<'_>) -> Result<()> {
        match entry {
            // Directories/symlinks are always "binary" (git's is_binary defaults
            // to 1 and is only cleared for regular files).
            ArchiveEntry::Directory { path } => self.write_entry(&path, 0o40000, &[], true),
            ArchiveEntry::File {
                path,
                mode,
                body,
                is_binary,
                ..
            } => self.write_entry(&path, mode, &body, is_binary),
            ArchiveEntry::Symlink { path, target, .. } => {
                self.write_entry(&path, 0o120000, target, true)
            }
        }
    }
}

impl<W: Write + ?Sized> ZipSink<'_, W> {
    /// Mirror of upstream `write_zip_entry`. `mode` is the raw git tree mode;
    /// `body` is the (already converted) file/symlink content, empty for
    /// directories. `is_binary` is git's `entry_is_binary` classification, used
    /// for the central-directory "is text" internal-attribute bit.
    fn write_entry(&mut self, path: &[u8], mode: u32, body: &[u8], is_binary: bool) -> Result<()> {
        let offset = self.offset;
        let pathlen = path.len();

        let mut flags: u16 = 0;
        if !has_only_ascii(path) && is_utf8(path) {
            flags |= ZIP_UTF8;
        }

        let is_dir = mode == 0o40000 || mode == 0o160000; // tree or gitlink
        let is_symlink = mode == 0o120000;
        let is_exec = mode & 0o111 != 0;

        let mut creator_version: u16 = 0;
        let attr2: u32 = if is_dir {
            16
        } else if is_symlink {
            creator_version = 0x0317;
            (mode | 0o777) << 16
        } else if is_exec {
            creator_version = 0x0317;
            mode << 16
        } else {
            0
        };
        if creator_version > self.max_creator_version {
            self.max_creator_version = creator_version;
        }

        let size = body.len() as u64;
        let crc = crc32fast::hash(body);

        // Method selection (store for dirs/symlinks/empty/-0; deflate otherwise,
        // falling back to store when deflate fails to shrink the data).
        let mut method = ZIP_METHOD_STORE;
        let deflated = if !is_dir && !is_symlink && self.compression_level != 0 && size > 0 {
            deflate_raw(body, self.compression_level)
                .filter(|compressed| (compressed.len() as u64) < size)
        } else {
            None
        };
        let out: &[u8] = match &deflated {
            Some(compressed) => {
                method = ZIP_METHOD_DEFLATE;
                compressed
            }
            None => body,
        };
        let compressed_size = out.len() as u64;

        let version_needed: u16 = 10;

        // --- local file header (magic 0x04034b50) ---
        let mut header = Vec::with_capacity(30);
        header.extend_from_slice(&0x04034b50u32.to_le_bytes());
        header.extend_from_slice(&version_needed.to_le_bytes());
        header.extend_from_slice(&flags.to_le_bytes());
        header.extend_from_slice(&method.to_le_bytes());
        header.extend_from_slice(&self.zip_time.to_le_bytes());
        header.extend_from_slice(&self.zip_date.to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&(compressed_size as u32).to_le_bytes());
        header.extend_from_slice(&(size as u32).to_le_bytes());
        header.extend_from_slice(&(pathlen as u16).to_le_bytes());
        header.extend_from_slice(&ZIP_EXTRA_MTIME_SIZE.to_le_bytes());
        self.writer.write_all(&header)?;
        self.offset += header.len() as u64;
        self.writer.write_all(path)?;
        self.offset += pathlen as u64;
        let extra = self.mtime_extra();
        self.writer.write_all(&extra)?;
        self.offset += extra.len() as u64;

        if compressed_size > 0 {
            self.writer.write_all(out)?;
            self.offset += compressed_size;
        }

        // --- central directory header (magic 0x02014b50) ---
        let cd = &mut self.central_dir;
        cd.extend_from_slice(&0x02014b50u32.to_le_bytes());
        cd.extend_from_slice(&creator_version.to_le_bytes());
        cd.extend_from_slice(&version_needed.to_le_bytes());
        cd.extend_from_slice(&flags.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&self.zip_time.to_le_bytes());
        cd.extend_from_slice(&self.zip_date.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(compressed_size as u32).to_le_bytes());
        cd.extend_from_slice(&(size as u32).to_le_bytes());
        cd.extend_from_slice(&(pathlen as u16).to_le_bytes());
        cd.extend_from_slice(&ZIP_EXTRA_MTIME_SIZE.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes()); // comment length
        cd.extend_from_slice(&0u16.to_le_bytes()); // disk
        // internal attributes: bit 0 = "is text". git sets `!is_binary`, where
        // `is_binary` follows the path's `diff` userdiff driver (or content
        // auto-detection). `unzip -a` reads this bit to decide EOL conversion.
        let internal_attrs: u16 = u16::from(!is_binary);
        cd.extend_from_slice(&internal_attrs.to_le_bytes());
        cd.extend_from_slice(&attr2.to_le_bytes()); // external attributes
        cd.extend_from_slice(&(offset as u32).to_le_bytes());
        cd.extend_from_slice(path);
        cd.extend_from_slice(&extra);

        self.entries += 1;
        Ok(())
    }

    fn mtime_extra(&self) -> [u8; ZIP_EXTRA_MTIME_SIZE as usize] {
        let mut extra = [0u8; ZIP_EXTRA_MTIME_SIZE as usize];
        extra[0..2].copy_from_slice(&0x5455u16.to_le_bytes());
        extra[2..4].copy_from_slice(&ZIP_EXTRA_MTIME_PAYLOAD_SIZE.to_le_bytes());
        extra[4] = 1; // just mtime
        extra[5..9].copy_from_slice(&self.mtime.to_le_bytes());
        extra
    }

    /// End-of-central-directory record (magic `0x06054b50`). `commit_id`, when
    /// present, becomes the archive comment (the hex object id).
    fn write_trailer(&mut self, commit_id: Option<&ObjectId>, format: ObjectFormat) -> Result<()> {
        self.writer.write_all(&self.central_dir)?;
        let central_dir_size = self.central_dir.len() as u64;
        let central_dir_offset = self.offset;
        let comment = commit_id.map(|oid| oid.to_string().into_bytes());
        let comment_len = comment.as_ref().map_or(0, |c| c.len());
        let mut clamped = false;
        let entries = clamp_u16(self.entries, &mut clamped);
        let central_dir_size_32 = clamp_u32(central_dir_size, &mut clamped);
        let central_dir_offset_32 = clamp_u32(central_dir_offset, &mut clamped);

        if clamped {
            self.write_zip64_trailer(central_dir_size, central_dir_offset)?;
        }

        let mut trailer = Vec::with_capacity(22);
        trailer.extend_from_slice(&0x06054b50u32.to_le_bytes());
        trailer.extend_from_slice(&0u16.to_le_bytes()); // disk
        trailer.extend_from_slice(&0u16.to_le_bytes()); // dir start disk
        trailer.extend_from_slice(&entries.to_le_bytes());
        trailer.extend_from_slice(&entries.to_le_bytes());
        trailer.extend_from_slice(&central_dir_size_32.to_le_bytes());
        trailer.extend_from_slice(&central_dir_offset_32.to_le_bytes());
        trailer.extend_from_slice(&(comment_len as u16).to_le_bytes());
        self.writer.write_all(&trailer)?;
        if let Some(comment) = comment {
            self.writer.write_all(&comment)?;
        }
        let _ = format;
        Ok(())
    }

    fn write_zip64_trailer(
        &mut self,
        central_dir_size: u64,
        central_dir_offset: u64,
    ) -> Result<()> {
        let zip64_offset = central_dir_offset + central_dir_size;

        let mut trailer = Vec::with_capacity(56);
        trailer.extend_from_slice(&0x06064b50u32.to_le_bytes());
        trailer.extend_from_slice(&ZIP64_END_OF_CENTRAL_DIRECTORY_RECORD_SIZE.to_le_bytes());
        trailer.extend_from_slice(&self.max_creator_version.to_le_bytes());
        trailer.extend_from_slice(&45u16.to_le_bytes());
        trailer.extend_from_slice(&0u32.to_le_bytes()); // disk
        trailer.extend_from_slice(&0u32.to_le_bytes()); // dir start disk
        trailer.extend_from_slice(&self.entries.to_le_bytes());
        trailer.extend_from_slice(&self.entries.to_le_bytes());
        trailer.extend_from_slice(&central_dir_size.to_le_bytes());
        trailer.extend_from_slice(&central_dir_offset.to_le_bytes());
        self.writer.write_all(&trailer)?;

        let mut locator = Vec::with_capacity(20);
        locator.extend_from_slice(&0x07064b50u32.to_le_bytes());
        locator.extend_from_slice(&0u32.to_le_bytes()); // zip64 trailer disk
        locator.extend_from_slice(&zip64_offset.to_le_bytes());
        locator.extend_from_slice(&1u32.to_le_bytes()); // number of disks
        self.writer.write_all(&locator)?;
        Ok(())
    }
}

fn clamp_u16(value: u64, clamped: &mut bool) -> u16 {
    if value > ZIP_MAX_16 {
        *clamped = true;
        u16::MAX
    } else {
        value as u16
    }
}

fn clamp_u32(value: u64, clamped: &mut bool) -> u32 {
    if value > ZIP_MAX_32 {
        *clamped = true;
        u32::MAX
    } else {
        value as u32
    }
}

/// Raw deflate (no zlib header), matching git's `zlib_deflate_raw`. Returns
/// `None` on a compression error so the caller can fall back to store.
fn deflate_raw(data: &[u8], level: u32) -> Option<Vec<u8>> {
    let level = Compression::new(level.min(9));
    let mut compressor = Compress::new(level, false);
    let bound = data.len() + (data.len() >> 12) + (data.len() >> 14) + (data.len() >> 25) + 13;
    let mut out = Vec::with_capacity(bound);
    let status = compressor
        .compress_vec(data, &mut out, FlushCompress::Finish)
        .ok()?;
    if status != Status::StreamEnd || compressor.total_in() != data.len() as u64 {
        return None;
    }
    Some(out)
}

/// DOS date/time fields, mirroring upstream `dos_time`. With `TZ=UTC` the
/// broken-down time is UTC. Returns `(date, time)`.
fn dos_time(timestamp: u64) -> (u16, u16) {
    let tm = gmtime(timestamp as i64);
    let date =
        (tm.mday as u16) + ((tm.mon as u16 + 1) * 32) + (((tm.year + 1900 - 1980) as u16) * 512);
    let time = (tm.sec as u16 / 2) + (tm.min as u16 * 32) + (tm.hour as u16 * 2048);
    (date, time)
}

struct BrokenTime {
    sec: i64,
    min: i64,
    hour: i64,
    mday: i64,
    mon: i64, // 0-based
    year: i64,
}

/// Civil-from-days algorithm (Howard Hinnant). Converts a unix timestamp to a
/// UTC broken-down time without libc, so it is timezone-independent and matches
/// the test harness's `TZ=UTC`.
fn gmtime(secs: i64) -> BrokenTime {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let mday = doy - (153 * mp + 2) / 5 + 1;
    let mon = if mp < 10 { mp + 3 } else { mp - 9 }; // 1..=12
    let year = if mon <= 2 { year + 1 } else { year };

    BrokenTime {
        sec,
        min,
        hour,
        mday,
        mon: mon - 1, // git uses tm_mon (0-based) and adds 1
        year: year - 1900,
    }
}

fn has_only_ascii(s: &[u8]) -> bool {
    s.iter().all(|b| b.is_ascii())
}

/// Minimal UTF-8 validity check (git's `is_utf8`).
fn is_utf8(s: &[u8]) -> bool {
    std::str::from_utf8(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u16(data: &[u8], offset: usize) -> u16 {
        sley_core::primitives::get_u16_le(data, offset).expect("test data bounds")
    }

    fn read_u32(data: &[u8], offset: usize) -> u32 {
        sley_core::primitives::get_u32_le(data, offset).expect("test data bounds")
    }

    fn read_u64(data: &[u8], offset: usize) -> u64 {
        sley_core::primitives::get_u64_le(data, offset).expect("test data bounds")
    }

    #[test]
    fn trailer_uses_zip64_when_entry_count_overflows_classic_eocd() {
        let mut archive = Vec::new();
        let central_dir = b"central".to_vec();
        let mut sink = ZipSink {
            writer: &mut archive,
            central_dir,
            offset: 123,
            entries: 65_792,
            max_creator_version: 0,
            mtime: 0,
            zip_date: 0,
            zip_time: 0,
            compression_level: 0,
        };

        sink.write_trailer(None, ObjectFormat::Sha1)
            .expect("trailer should be written");

        assert_eq!(&archive[..7], b"central");

        let zip64 = 7;
        assert_eq!(read_u32(&archive, zip64), 0x06064b50);
        assert_eq!(
            read_u64(&archive, zip64 + 4),
            ZIP64_END_OF_CENTRAL_DIRECTORY_RECORD_SIZE
        );
        assert_eq!(read_u16(&archive, zip64 + 14), 45);
        assert_eq!(read_u64(&archive, zip64 + 24), 65_792);
        assert_eq!(read_u64(&archive, zip64 + 32), 65_792);
        assert_eq!(read_u64(&archive, zip64 + 40), 7);
        assert_eq!(read_u64(&archive, zip64 + 48), 123);

        let locator = zip64 + 56;
        assert_eq!(read_u32(&archive, locator), 0x07064b50);
        assert_eq!(read_u64(&archive, locator + 8), 130);

        let eocd = locator + 20;
        assert_eq!(read_u32(&archive, eocd), 0x06054b50);
        assert_eq!(read_u16(&archive, eocd + 8), u16::MAX);
        assert_eq!(read_u16(&archive, eocd + 10), u16::MAX);
        assert_eq!(read_u32(&archive, eocd + 12), 7);
        assert_eq!(read_u32(&archive, eocd + 16), 123);
    }
}
