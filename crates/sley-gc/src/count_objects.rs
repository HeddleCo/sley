//! `count-objects` aggregation machinery: loose/pack scanning, garbage
//! detection, pack-index correspondence warnings, and the packed-object
//! lookup used to attribute prune-packable loose objects. Byte-exact stdout
//! formatting stays in the CLI; this module only aggregates.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_odb::repository_objects_dir;

/// Shared stem record for pack-file bookkeeping (also used by gc's
/// garbage-clean pass).
#[derive(Debug, Clone, Default)]
pub struct CountObjectsStats {
    pub count: u64,
    pub size_kib: u64,
    pub in_pack: u64,
    pub packs: u64,
    pub size_pack_bytes: u64,
    pub prune_packable: u64,
    pub garbage: u64,
    pub size_garbage_bytes: u64,
    pub alternates: Vec<String>,
}

pub fn count_objects_stats(git_dir: &Path, format: ObjectFormat) -> Result<CountObjectsStats> {
    let objects_dir = repository_objects_dir(git_dir);
    let mut stats = CountObjectsStats::default();
    if !objects_dir.exists() {
        return Ok(stats);
    }
    stats.alternates = count_objects_alternates(&objects_dir)?;
    let default_objects_dir = git_dir.join("objects");
    let display_root = if objects_dir == default_objects_dir {
        git_dir.parent().unwrap_or(git_dir)
    } else {
        objects_dir.parent().unwrap_or(&objects_dir)
    };
    let pack_indexes = count_pack_objects(&objects_dir.join("pack"), format, &mut stats)?;
    let mut packed_lookup = CountPackedObjectLookup::new(format, pack_indexes);
    let hex_len = format.hex_len();
    for entry in fs::read_dir(&objects_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "info" || name == "pack" {
            continue;
        }
        if entry.metadata()?.is_dir()
            && name.len() == 2
            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            count_loose_object_directory(
                &path,
                display_root,
                &name,
                format,
                hex_len,
                &mut packed_lookup,
                &mut stats,
            )?;
        }
    }
    Ok(stats)
}

fn count_objects_alternates(objects_dir: &Path) -> Result<Vec<String>> {
    let alternates_path = objects_dir.join("info").join("alternates");
    let Ok(contents) = fs::read(&alternates_path) else {
        return Ok(Vec::new());
    };
    let mut alternates = Vec::new();
    for raw in contents.split(|byte| *byte == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let value =
            std::str::from_utf8(line).map_err(|err| GitError::InvalidFormat(err.to_string()))?;
        let path = Path::new(value);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            objects_dir.join(path)
        };
        let display = fs::canonicalize(&absolute).unwrap_or(absolute);
        alternates.push(display.to_string_lossy().into_owned());
    }
    Ok(alternates)
}

fn count_loose_object_directory(
    dir: &Path,
    display_root: &Path,
    fanout: &str,
    format: ObjectFormat,
    hex_len: usize,
    packed_lookup: &mut CountPackedObjectLookup,
    stats: &mut CountObjectsStats,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if metadata.is_file()
            && name.len() == hex_len - 2
            && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            let oid = ObjectId::from_hex(format, &format!("{fanout}{name}"))?;
            stats.count += 1;
            stats.size_kib += filesystem_size_kib(&metadata);
            if packed_lookup.contains(&oid)? {
                stats.prune_packable += 1;
            }
        } else {
            let entry_path = entry.path();
            let display_path = entry_path
                .strip_prefix(display_root)
                .unwrap_or(entry_path.as_path());
            eprintln!("warning: garbage found: {}", display_path.display());
            stats.garbage += 1;
            stats.size_garbage_bytes += metadata.len();
        }
    }
    Ok(())
}

fn count_pack_objects(
    pack_dir: &Path,
    format: ObjectFormat,
    stats: &mut CountObjectsStats,
) -> Result<Vec<CountPackIndexSummary>> {
    let mut pack_indexes = Vec::new();
    if !pack_dir.exists() {
        return Ok(pack_indexes);
    }
    let display_root = pack_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap_or(pack_dir);
    let mut stems: BTreeMap<String, CountPackStem> = BTreeMap::new();
    for entry in fs::read_dir(pack_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("pack") => {
                stats.packs += 1;
                stats.size_pack_bytes += metadata.len();
                stems.entry(stem).or_default().pack = Some(path);
            }
            Some("idx") => {
                let summary = count_pack_index_summary(&path, &metadata, format)?;
                if let Some(summary) = summary {
                    stats.size_pack_bytes += metadata.len();
                    stats.in_pack += u64::from(summary.object_count);
                    pack_indexes.push(summary);
                }
                stems.entry(stem).or_default().idx = Some(path);
            }
            Some("keep") => {
                stems.entry(stem).or_default().keep = Some(path);
            }
            Some("rev" | "bitmap" | "mtimes" | "promisor") => {}
            _ => count_pack_garbage(&path, &metadata, display_root, stats),
        }
    }
    for stem in stems.values() {
        match (&stem.pack, &stem.idx, &stem.keep) {
            (Some(pack), None, Some(keep)) => {
                count_pack_correspondence_warning("no corresponding .idx", keep, display_root);
                count_pack_correspondence_warning("no corresponding .idx", pack, display_root);
            }
            (Some(pack), None, None) => {
                count_pack_correspondence_warning("no corresponding .idx", pack, display_root);
            }
            (None, Some(idx), Some(keep)) => {
                count_pack_correspondence_warning("no corresponding .pack", idx, display_root);
                count_pack_correspondence_warning("no corresponding .pack", keep, display_root);
            }
            (None, Some(idx), None) => {
                count_pack_correspondence_warning("no corresponding .pack", idx, display_root);
            }
            (None, None, Some(keep)) => {
                count_pack_correspondence_warning(
                    "no corresponding .idx or .pack",
                    keep,
                    display_root,
                );
            }
            _ => {}
        }
    }
    Ok(pack_indexes)
}

#[derive(Debug, Default)]
pub struct CountPackStem {
    pub(crate) pack: Option<PathBuf>,
    pub(crate) idx: Option<PathBuf>,
    pub(crate) keep: Option<PathBuf>,
}

fn count_pack_garbage(
    path: &Path,
    _metadata: &fs::Metadata,
    display_root: &Path,
    _stats: &mut CountObjectsStats,
) {
    let display_path = path.strip_prefix(display_root).unwrap_or(path);
    eprintln!("warning: garbage found: {}", display_path.display());
}

fn count_pack_correspondence_warning(message: &str, path: &Path, display_root: &Path) {
    let display_path = path.strip_prefix(display_root).unwrap_or(path);
    eprintln!("warning: {message}: {}", display_path.display());
}

#[derive(Debug, Clone)]
pub(crate) struct CountPackIndexSummary {
    path: PathBuf,
    object_count: u32,
}

#[derive(Debug)]
struct CountPackedObjectLookup {
    format: ObjectFormat,
    summaries: Vec<CountPackIndexSummary>,
    indexes: Option<Vec<CountPackIndexLookup>>,
}

impl CountPackedObjectLookup {
    fn new(format: ObjectFormat, summaries: Vec<CountPackIndexSummary>) -> Self {
        Self {
            format,
            summaries,
            indexes: None,
        }
    }

    fn contains(&mut self, oid: &ObjectId) -> Result<bool> {
        if self.summaries.is_empty() {
            return Ok(false);
        }
        if self.indexes.is_none() {
            self.indexes = Some(load_count_pack_index_lookups(
                self.format,
                self.summaries.as_slice(),
            )?);
        }
        Ok(self
            .indexes
            .as_ref()
            .expect("count pack indexes are loaded")
            .iter()
            .any(|index| index.contains(oid)))
    }
}

#[derive(Debug)]
struct CountPackIndexLookup {
    format: ObjectFormat,
    fanout: [u32; 256],
    bytes: Vec<u8>,
    layout: CountPackIndexLayout,
}

#[derive(Debug)]
enum CountPackIndexLayout {
    V1 {
        entry_table_start: usize,
        entry_len: usize,
    },
    V2 {
        oid_table_start: usize,
    },
}

impl CountPackIndexLookup {
    fn parse(bytes: Vec<u8>, format: ObjectFormat) -> Result<Self> {
        let metadata = count_pack_index_metadata(&bytes, format)?;
        if count_pack_index_min_len(&metadata, format)? > bytes.len() {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        Ok(Self {
            format,
            fanout: metadata.fanout,
            bytes,
            layout: metadata.layout,
        })
    }

    fn contains(&self, oid: &ObjectId) -> bool {
        if oid.format() != self.format {
            return false;
        }
        let oid_bytes = oid.as_bytes();
        let bucket = usize::from(oid_bytes[0]);
        let start = if bucket == 0 {
            0
        } else {
            self.fanout[bucket - 1] as usize
        };
        let end = self.fanout[bucket] as usize;
        if start == end {
            return false;
        }
        let mut low = start;
        let mut high = end;
        while low < high {
            let mid = low + (high - low) / 2;
            match self.oid_at(mid).cmp(oid_bytes) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Equal => return true,
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        false
    }

    fn oid_at(&self, idx: usize) -> &[u8] {
        match self.layout {
            CountPackIndexLayout::V1 {
                entry_table_start,
                entry_len,
            } => {
                let start = entry_table_start + idx * entry_len + 4;
                &self.bytes[start..start + self.format.raw_len()]
            }
            CountPackIndexLayout::V2 { oid_table_start } => {
                let start = oid_table_start + idx * self.format.raw_len();
                &self.bytes[start..start + self.format.raw_len()]
            }
        }
    }
}

#[derive(Debug)]
struct CountPackIndexMetadata {
    object_count: u32,
    fanout: [u32; 256],
    layout: CountPackIndexLayout,
}

pub(crate) fn count_pack_index_summary(
    path: &Path,
    metadata: &fs::Metadata,
    format: ObjectFormat,
) -> Result<Option<CountPackIndexSummary>> {
    let len = usize::try_from(metadata.len())
        .map_err(|_| GitError::InvalidFormat("pack index is too large".into()))?;
    let prefix_len = if len >= 4 && count_pack_index_has_v2_magic(path)? {
        8 + 256 * 4
    } else {
        256 * 4
    };
    if len < prefix_len {
        eprintln!(
            "error: index file {} is too small",
            count_pack_display_path(path).display()
        );
        return Ok(None);
    }
    let mut file = fs::File::open(path)?;
    let mut prefix = vec![0u8; prefix_len];
    match io::Read::read_exact(&mut file, &mut prefix) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    match count_pack_index_prefix_metadata(&prefix, format) {
        Ok(index) if count_pack_index_min_len(&index, format)? <= len => {
            Ok(Some(CountPackIndexSummary {
                path: path.to_path_buf(),
                object_count: index.object_count,
            }))
        }
        _ => Ok(None),
    }
}

fn count_pack_index_has_v2_magic(path: &Path) -> Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    match io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) => Ok(magic == [0xff, b't', b'O', b'c']),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn count_pack_display_path(path: &Path) -> &Path {
    let display_root = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new(""));
    path.strip_prefix(display_root).unwrap_or(path)
}

fn load_count_pack_index_lookups(
    format: ObjectFormat,
    summaries: &[CountPackIndexSummary],
) -> Result<Vec<CountPackIndexLookup>> {
    let mut indexes = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let bytes = fs::read(&summary.path)?;
        if let Ok(index) = CountPackIndexLookup::parse(bytes, format) {
            indexes.push(index);
        }
    }
    Ok(indexes)
}

fn count_pack_index_metadata(bytes: &[u8], format: ObjectFormat) -> Result<CountPackIndexMetadata> {
    let hash_len = format.raw_len();
    if bytes.len() < 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    if bytes[..4] == [0xff, b't', b'O', b'c'] {
        if bytes.len() < 8 + 256 * 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = sley_core::primitives::u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let (fanout, object_count) = count_pack_index_fanout(&bytes[8..8 + 256 * 4])?;
        let oid_table_start = 8 + 256 * 4;
        let oid_table = count_checked_range(oid_table_start, object_count as usize, hash_len)?;
        let crc_table = count_checked_range(oid_table.end, object_count as usize, 4)?;
        let small_offset_table = count_checked_range(crc_table.end, object_count as usize, 4)?;
        if bytes.len() < small_offset_table.end {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        return Ok(CountPackIndexMetadata {
            object_count,
            fanout,
            layout: CountPackIndexLayout::V2 { oid_table_start },
        });
    }

    if bytes.len() < 256 * 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    let (fanout, object_count) = count_pack_index_fanout(&bytes[..256 * 4])?;
    let entry_table_start = 256 * 4;
    let entry_len = hash_len
        .checked_add(4)
        .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
    let entry_table = count_checked_range(entry_table_start, object_count as usize, entry_len)?;
    if bytes.len() < entry_table.end {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    Ok(CountPackIndexMetadata {
        object_count,
        fanout,
        layout: CountPackIndexLayout::V1 {
            entry_table_start,
            entry_len,
        },
    })
}

fn count_pack_index_prefix_metadata(
    bytes: &[u8],
    format: ObjectFormat,
) -> Result<CountPackIndexMetadata> {
    if bytes.len() < 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    if bytes[..4] == [0xff, b't', b'O', b'c'] {
        if bytes.len() < 8 + 256 * 4 {
            return Err(GitError::InvalidFormat("pack index too short".into()));
        }
        let version = sley_core::primitives::u32_be(&bytes[4..8]);
        if version != 2 {
            return Err(GitError::Unsupported(format!(
                "pack index version {version}"
            )));
        }
        let (fanout, object_count) = count_pack_index_fanout(&bytes[8..8 + 256 * 4])?;
        return Ok(CountPackIndexMetadata {
            object_count,
            fanout,
            layout: CountPackIndexLayout::V2 {
                oid_table_start: 8 + 256 * 4,
            },
        });
    }

    if bytes.len() < 256 * 4 {
        return Err(GitError::InvalidFormat("pack index too short".into()));
    }
    let (fanout, object_count) = count_pack_index_fanout(&bytes[..256 * 4])?;
    let entry_len = format
        .raw_len()
        .checked_add(4)
        .ok_or_else(|| GitError::InvalidFormat("pack index entry length overflow".into()))?;
    Ok(CountPackIndexMetadata {
        object_count,
        fanout,
        layout: CountPackIndexLayout::V1 {
            entry_table_start: 256 * 4,
            entry_len,
        },
    })
}

fn count_pack_index_min_len(index: &CountPackIndexMetadata, format: ObjectFormat) -> Result<usize> {
    let hash_len = format.raw_len();
    match index.layout {
        CountPackIndexLayout::V1 {
            entry_table_start,
            entry_len,
        } => count_checked_range(entry_table_start, index.object_count as usize, entry_len)?
            .end
            .checked_add(hash_len * 2)
            .ok_or_else(|| GitError::InvalidFormat("pack index length overflow".into())),
        CountPackIndexLayout::V2 { oid_table_start } => {
            let oid_table =
                count_checked_range(oid_table_start, index.object_count as usize, hash_len)?;
            let crc_table = count_checked_range(oid_table.end, index.object_count as usize, 4)?;
            let small_offset_table =
                count_checked_range(crc_table.end, index.object_count as usize, 4)?;
            small_offset_table
                .end
                .checked_add(hash_len * 2)
                .ok_or_else(|| GitError::InvalidFormat("pack index length overflow".into()))
        }
    }
}

fn count_pack_index_fanout(bytes: &[u8]) -> Result<([u32; 256], u32)> {
    let mut fanout = [0u32; 256];
    let mut previous = 0u32;
    for (idx, slot) in fanout.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = sley_core::primitives::u32_be(&bytes[start..start + 4]);
        if *slot < previous {
            return Err(GitError::InvalidFormat(
                "pack index fanout is not monotonic".into(),
            ));
        }
        previous = *slot;
    }
    Ok((fanout, fanout[255]))
}

fn count_checked_range(start: usize, count: usize, width: usize) -> Result<std::ops::Range<usize>> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| GitError::InvalidFormat("pack index table length overflow".into()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| GitError::InvalidFormat("pack index table offset overflow".into()))?;
    Ok(start..end)
}

pub fn count_objects_size(size_kib: u64, human_readable: bool) -> String {
    if human_readable {
        count_objects_human_size(size_kib)
    } else {
        size_kib.to_string()
    }
}

pub fn count_objects_pack_size(size_bytes: u64, human_readable: bool) -> String {
    if human_readable {
        count_objects_human_bytes(size_bytes)
    } else {
        (size_bytes / 1024).to_string()
    }
}

pub fn count_objects_human_size(size_kib: u64) -> String {
    if size_kib == 0 {
        return "0 bytes".to_string();
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut size = size_kib as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

#[cfg(unix)]
fn filesystem_size_kib(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().div_ceil(2)
}

#[cfg(not(unix))]
fn filesystem_size_kib(metadata: &fs::Metadata) -> u64 {
    metadata.len().div_ceil(1024)
}

fn count_objects_human_bytes(size_bytes: u64) -> String {
    if size_bytes == 0 {
        return "0 bytes".to_string();
    }
    if size_bytes < 1024 {
        return format!("{size_bytes} bytes");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut size = size_bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}
