use sley_core::{GitError, ObjectFormat, ObjectId, Result};
use sley_pack::{
    MultiPackBitmapPack, MultiPackIndex, MultiPackIndexEntry, PackBitmapIndex, PackBitmapWriter,
    PackIndex, PackReverseIndex,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    BitmapPseudoMergeGroup, FileObjectDatabase, ReachabilityBitmapOptions,
    build_midx_bitmap_with_options,
};

/// Policy for a requested MIDX bitmap whose selected tips do not have complete
/// closure in the packs represented by the new MIDX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingMidxBitmapPolicy {
    /// Refuse the MIDX write. This is the ordinary `multi-pack-index write`
    /// behavior: the MIDX itself must not land when its bitmap cannot be built.
    Error,
    /// Write a type-only empty bitmap. Incremental MIDX layers use this
    /// compatibility fallback while their object closure can span older layers.
    WriteEmpty,
}

/// Runtime inputs needed only when a MIDX bitmap is actually built.
///
/// The engine asks for these lazily through the provider passed to
/// [`build_multi_pack_index_layer`]. That keeps ref/config/revision resolution
/// outside the object database and avoids doing it for an unchanged MIDX.
#[derive(Debug, Clone)]
pub struct MultiPackIndexBitmapInputs {
    pub preferred_tips: HashSet<ObjectId>,
    pub pseudo_merge_groups: Vec<BitmapPseudoMergeGroup>,
    pub write_lookup_table: bool,
    pub write_hash_cache: bool,
    pub restrict_to_tips: bool,
    pub write_reverse_index: bool,
    pub missing_closure: MissingMidxBitmapPolicy,
}

impl Default for MultiPackIndexBitmapInputs {
    fn default() -> Self {
        Self {
            preferred_tips: HashSet::new(),
            pseudo_merge_groups: Vec::new(),
            write_lookup_table: false,
            write_hash_cache: true,
            restrict_to_tips: false,
            write_reverse_index: false,
            missing_closure: MissingMidxBitmapPolicy::Error,
        }
    }
}

/// Semantic inputs for building one MIDX layer from pack indexes.
#[derive(Debug, Clone)]
pub struct MultiPackIndexLayerOptions {
    pub object_dir: PathBuf,
    pub format: ObjectFormat,
    /// MIDX format version (`1` for ordinary/incremental layers, `2` for an
    /// incremental compaction layer).
    pub version: u8,
    /// Pack index basenames, in the order exposed by the CLI input.
    pub pack_names: Vec<String>,
    /// Objects already represented by older incremental layers.
    pub excluded_oids: HashSet<ObjectId>,
    pub write_bitmap: bool,
    pub preferred_pack_name: Option<String>,
    /// Preserve an existing byte-identical ordinary MIDX (and bitmap) without
    /// resolving bitmap inputs or rewriting files.
    pub skip_if_unchanged: bool,
}

/// Inputs for serializing a MIDX layer whose pack table and object locations
/// have already been selected by an incremental compaction operation.
#[derive(Debug, Clone)]
pub struct MultiPackIndexEntryLayerOptions {
    pub object_dir: PathBuf,
    pub format: ObjectFormat,
    pub version: u8,
    pub pack_names: Vec<String>,
    pub objects: Vec<MultiPackIndexEntry>,
    pub write_bitmap: bool,
    pub preferred_pack: Option<u32>,
}

impl MultiPackIndexLayerOptions {
    pub fn new(
        object_dir: impl Into<PathBuf>,
        format: ObjectFormat,
        pack_names: Vec<String>,
    ) -> Self {
        Self {
            object_dir: object_dir.into(),
            format,
            version: 1,
            pack_names,
            excluded_oids: HashSet::new(),
            write_bitmap: false,
            preferred_pack_name: None,
            skip_if_unchanged: false,
        }
    }
}

/// Non-fatal diagnostics discovered while selecting a MIDX layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiPackIndexEvent {
    UnknownPreferredPack(String),
    RefusingEmptyBitmap,
}

/// Engine-level failures which need command-specific exit codes/rendering.
#[derive(Debug)]
pub enum MultiPackIndexLayerError {
    Source(GitError),
    CouldNotLoadPack,
    EmptyPreferredPack(PathBuf),
    BitmapUnavailable,
}

impl std::fmt::Display for MultiPackIndexLayerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(formatter),
            Self::CouldNotLoadPack => formatter.write_str("could not load pack"),
            Self::EmptyPreferredPack(path) => write!(
                formatter,
                "cannot select preferred pack {} with no objects",
                path.display()
            ),
            Self::BitmapUnavailable => formatter.write_str("could not write multi-pack bitmap"),
        }
    }
}

impl std::error::Error for MultiPackIndexLayerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::CouldNotLoadPack | Self::EmptyPreferredPack(_) | Self::BitmapUnavailable => None,
        }
    }
}

impl From<GitError> for MultiPackIndexLayerError {
    fn from(value: GitError) -> Self {
        Self::Source(value)
    }
}

impl From<io::Error> for MultiPackIndexLayerError {
    fn from(value: io::Error) -> Self {
        Self::Source(value.into())
    }
}

/// Fully materialized bytes and selection metadata for one MIDX layer.
#[derive(Debug, Clone)]
pub struct MultiPackIndexLayerOutcome {
    pub checksum: ObjectId,
    pub midx: Vec<u8>,
    pub bitmap: Option<Vec<u8>>,
    pub reverse_index: Option<Vec<u8>>,
    pub preferred_pack: Option<u32>,
    pub wrote_bitmap: bool,
    /// True when the engine found the exact ordinary MIDX/bitmap already on
    /// disk. In that case callers must leave every file untouched.
    pub unchanged: bool,
}

/// Build an ordinary or incremental multi-pack-index layer.
///
/// Pack discovery, argument parsing, live-ref selection and diagnostic
/// rendering remain caller concerns. This operation owns the pack-index read,
/// preferred/duplicate selection, MIDX serialization, bitmap construction and
/// optional reverse-index construction. `bitmap_inputs` is invoked only if a
/// bitmap is still needed after selection and the unchanged-file fast path.
pub fn build_multi_pack_index_layer<F, E>(
    mut options: MultiPackIndexLayerOptions,
    bitmap_inputs: F,
    mut emit: E,
) -> std::result::Result<MultiPackIndexLayerOutcome, MultiPackIndexLayerError>
where
    F: FnOnce(&FileObjectDatabase) -> Result<MultiPackIndexBitmapInputs>,
    E: FnMut(MultiPackIndexEvent),
{
    let pack_dir = options.object_dir.join("pack");
    let pack_mtimes: Vec<SystemTime> = options
        .pack_names
        .iter()
        .map(|name| {
            fs::metadata(pack_dir.join(name).with_extension("pack"))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH)
        })
        .collect();
    let pack_mtime = |pack_int_id: u32| -> SystemTime {
        pack_mtimes
            .get(pack_int_id as usize)
            .copied()
            .unwrap_or(UNIX_EPOCH)
    };

    let mut objects = Vec::new();
    let mut pack_object_counts = vec![0usize; options.pack_names.len()];
    for (pack_int_id, pack_name) in options.pack_names.iter().enumerate() {
        let pack_path = pack_dir.join(pack_name).with_extension("pack");
        if !pack_path.exists() {
            return Err(MultiPackIndexLayerError::CouldNotLoadPack);
        }
        let index_bytes = fs::read(pack_dir.join(pack_name))?;
        let force_large_offset = pack_index_has_large_offset_area(&index_bytes, options.format);
        let index = PackIndex::parse_without_checksum(&index_bytes, options.format)?;
        pack_object_counts[pack_int_id] = index.entries.len();
        for entry in index.entries {
            if options.excluded_oids.contains(&entry.oid) {
                continue;
            }
            objects.push(MultiPackIndexEntry {
                oid: entry.oid,
                pack_int_id: pack_int_id as u32,
                offset: entry.offset,
                force_large_offset,
            });
        }
    }

    if options.write_bitmap && objects.is_empty() {
        emit(MultiPackIndexEvent::RefusingEmptyBitmap);
        options.write_bitmap = false;
    }

    let preferred_pack = match options.preferred_pack_name.as_deref() {
        Some(name) => {
            let normalized = name.strip_suffix(".pack").map(|stem| format!("{stem}.idx"));
            match options.pack_names.iter().position(|pack_name| {
                pack_name == name || Some(pack_name.as_str()) == normalized.as_deref()
            }) {
                Some(position) => {
                    if pack_object_counts.get(position).copied().unwrap_or(0) == 0 {
                        return Err(MultiPackIndexLayerError::EmptyPreferredPack(
                            pack_dir
                                .join(&options.pack_names[position])
                                .with_extension("pack"),
                        ));
                    }
                    Some(position as u32)
                }
                None => {
                    emit(MultiPackIndexEvent::UnknownPreferredPack(name.to_string()));
                    options.write_bitmap.then_some(0)
                }
            }
        }
        None if options.write_bitmap => {
            let mut preferred = 0u32;
            let mut oldest: Option<SystemTime> = None;
            for pack_int_id in 0..options.pack_names.len() as u32 {
                let mtime = pack_mtime(pack_int_id);
                if oldest.is_none_or(|current| mtime < current) {
                    oldest = Some(mtime);
                    preferred = pack_int_id;
                }
            }
            Some(preferred)
        }
        None => None,
    };

    // Resolve duplicates the way midx_oid_compare does: preferred pack, then
    // newest pack, then the lowest pack id.
    objects.sort_by(|left, right| {
        left.oid
            .as_bytes()
            .cmp(right.oid.as_bytes())
            .then_with(|| {
                let left_preferred = Some(left.pack_int_id) == preferred_pack;
                let right_preferred = Some(right.pack_int_id) == preferred_pack;
                right_preferred.cmp(&left_preferred)
            })
            .then_with(|| pack_mtime(right.pack_int_id).cmp(&pack_mtime(left.pack_int_id)))
            .then_with(|| left.pack_int_id.cmp(&right.pack_int_id))
    });
    objects.dedup_by(|next, kept| next.oid == kept.oid);

    build_selected_multi_pack_index_layer(
        options.object_dir,
        options.format,
        options.version,
        options.pack_names,
        objects,
        options.write_bitmap,
        preferred_pack,
        options.skip_if_unchanged,
        bitmap_inputs,
    )
}

/// Serialize one already-selected MIDX layer, including its optional bitmap
/// and reverse index. Incremental compaction uses this after remapping entries
/// from several source layers into a single pack table.
pub fn build_multi_pack_index_layer_from_entries<F>(
    options: MultiPackIndexEntryLayerOptions,
    bitmap_inputs: F,
) -> std::result::Result<MultiPackIndexLayerOutcome, MultiPackIndexLayerError>
where
    F: FnOnce(&FileObjectDatabase) -> Result<MultiPackIndexBitmapInputs>,
{
    build_selected_multi_pack_index_layer(
        options.object_dir,
        options.format,
        options.version,
        options.pack_names,
        options.objects,
        options.write_bitmap,
        options.preferred_pack,
        false,
        bitmap_inputs,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_selected_multi_pack_index_layer<F>(
    object_dir: PathBuf,
    format: ObjectFormat,
    version: u8,
    pack_names: Vec<String>,
    objects: Vec<MultiPackIndexEntry>,
    write_bitmap: bool,
    preferred_pack: Option<u32>,
    skip_if_unchanged: bool,
    bitmap_inputs: F,
) -> std::result::Result<MultiPackIndexLayerOutcome, MultiPackIndexLayerError>
where
    F: FnOnce(&FileObjectDatabase) -> Result<MultiPackIndexBitmapInputs>,
{
    let pack_dir = object_dir.join("pack");
    let bitmapped_packs = write_bitmap.then(|| {
        if version == 2 {
            incremental_compact_bitmapped_pack_ranges(pack_names.len(), &objects)
        } else {
            midx_bitmapped_pack_ranges(pack_names.len(), &objects, preferred_pack.unwrap_or(0))
        }
    });
    let midx = MultiPackIndex::write_with_bitmap_packs(
        format,
        version,
        &pack_names,
        &objects,
        write_bitmap.then(|| preferred_pack.unwrap_or(0)),
        bitmapped_packs.as_deref(),
    )?;
    let checksum = ObjectId::from_raw(format, &midx[midx.len() - format.raw_len()..])?;
    let bitmap_name = format!("multi-pack-index-{checksum}.bitmap");
    if skip_if_unchanged
        && fs::read(pack_dir.join("multi-pack-index")).is_ok_and(|existing| existing == midx)
        && (!write_bitmap || pack_dir.join(bitmap_name).exists())
    {
        return Ok(MultiPackIndexLayerOutcome {
            checksum,
            midx,
            bitmap: None,
            reverse_index: None,
            preferred_pack,
            wrote_bitmap: write_bitmap,
            unchanged: true,
        });
    }

    let db = FileObjectDatabase::new(object_dir, format);
    let (bitmap, reverse_index) = if write_bitmap {
        let inputs = bitmap_inputs(&db)?;
        let name_hash_cache = if inputs.write_hash_cache {
            Some(midx_bitmap_name_hash_cache(
                &pack_dir,
                &pack_names,
                &objects,
                format,
            )?)
        } else {
            None
        };
        let bitmap_options = ReachabilityBitmapOptions {
            write_lookup_table: inputs.write_lookup_table,
            name_hash_cache,
            restrict_to_tips: inputs.restrict_to_tips,
        };
        let bitmap = match build_midx_bitmap_with_options(
            &db,
            format,
            &objects,
            &checksum,
            preferred_pack.unwrap_or(0),
            &inputs.preferred_tips,
            &inputs.pseudo_merge_groups,
            &bitmap_options,
        )? {
            Some(bitmap) => bitmap,
            None if inputs.missing_closure == MissingMidxBitmapPolicy::WriteEmpty => {
                write_empty_midx_bitmap(&db, format, &objects, &checksum, preferred_pack)?
            }
            None => return Err(MultiPackIndexLayerError::BitmapUnavailable),
        };
        let reverse_index = if inputs.write_reverse_index {
            let mut pseudo: Vec<u32> = (0..objects.len() as u32).collect();
            let preferred = preferred_pack.unwrap_or(0);
            pseudo.sort_by_key(|&midx_pos| {
                let object = &objects[midx_pos as usize];
                (
                    object.pack_int_id != preferred,
                    object.pack_int_id,
                    object.offset,
                )
            });
            Some(PackReverseIndex::write(format, &pseudo, &checksum)?)
        } else {
            None
        };
        (Some(bitmap), reverse_index)
    } else {
        (None, None)
    };

    Ok(MultiPackIndexLayerOutcome {
        checksum,
        midx,
        bitmap,
        reverse_index,
        preferred_pack,
        wrote_bitmap: write_bitmap,
        unchanged: false,
    })
}

fn midx_bitmap_name_hash_cache(
    pack_dir: &Path,
    pack_names: &[String],
    objects: &[MultiPackIndexEntry],
    format: ObjectFormat,
) -> Result<Vec<u32>> {
    let mut by_oid = HashMap::new();
    for pack_name in pack_names {
        let index_path = pack_dir.join(pack_name);
        let bitmap_path = index_path.with_extension("bitmap");
        let Ok(index_bytes) = fs::read(&index_path) else {
            continue;
        };
        let Ok(index) = PackIndex::parse_without_checksum(&index_bytes, format) else {
            continue;
        };
        let Ok(bitmap_bytes) = fs::read(bitmap_path) else {
            continue;
        };
        let Ok(bitmap) = PackBitmapIndex::parse(&bitmap_bytes, format, index.entries.len()) else {
            continue;
        };
        let Some(cache) = bitmap.name_hash_cache else {
            continue;
        };
        let mut entries = index.entries;
        entries.sort_by(|left, right| left.oid.as_bytes().cmp(right.oid.as_bytes()));
        for (entry, hash) in entries.into_iter().zip(cache) {
            by_oid.entry(entry.oid).or_insert(hash);
        }
    }
    Ok(objects
        .iter()
        .map(|entry| by_oid.get(&entry.oid).copied().unwrap_or(0))
        .collect())
}

fn write_empty_midx_bitmap(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    objects: &[MultiPackIndexEntry],
    checksum: &ObjectId,
    preferred_pack: Option<u32>,
) -> Result<Vec<u8>> {
    let preferred = preferred_pack.unwrap_or(0);
    let mut pseudo: Vec<usize> = (0..objects.len()).collect();
    pseudo.sort_by_key(|&midx_pos| {
        let object = &objects[midx_pos];
        (
            object.pack_int_id != preferred,
            object.pack_int_id,
            object.offset,
        )
    });
    let mut object_types = Vec::with_capacity(objects.len());
    for midx_pos in pseudo {
        let oid = objects[midx_pos].oid;
        let (object_type, _size) = db.read_object_header(&oid)?.ok_or_else(|| {
            GitError::InvalidFormat(format!("object {oid} missing while writing bitmap"))
        })?;
        object_types.push(object_type);
    }
    PackBitmapWriter::new(format, *checksum, &object_types)?.write()
}

fn midx_bitmapped_pack_ranges(
    pack_count: usize,
    objects: &[MultiPackIndexEntry],
    preferred_pack: u32,
) -> Vec<MultiPackBitmapPack> {
    let mut ranges = vec![
        MultiPackBitmapPack {
            bitmap_pos: 0,
            bitmap_nr: 0,
        };
        pack_count
    ];
    let mut pseudo: Vec<usize> = (0..objects.len()).collect();
    pseudo.sort_by_key(|&midx_pos| {
        let object = &objects[midx_pos];
        (
            object.pack_int_id != preferred_pack,
            object.pack_int_id,
            object.offset,
        )
    });
    for (bitmap_pos, midx_pos) in pseudo.into_iter().enumerate() {
        let pack = &mut ranges[objects[midx_pos].pack_int_id as usize];
        if pack.bitmap_nr == 0 {
            pack.bitmap_pos = bitmap_pos as u32;
        }
        pack.bitmap_nr += 1;
    }
    ranges
}

fn incremental_compact_bitmapped_pack_ranges(
    pack_count: usize,
    objects: &[MultiPackIndexEntry],
) -> Vec<MultiPackBitmapPack> {
    let mut ranges = vec![
        MultiPackBitmapPack {
            bitmap_pos: 0,
            bitmap_nr: 0,
        };
        pack_count
    ];
    for object in objects {
        if let Some(pack) = ranges.get_mut(object.pack_int_id as usize) {
            pack.bitmap_nr += 1;
        }
    }
    ranges
}

fn pack_index_has_large_offset_area(bytes: &[u8], format: ObjectFormat) -> bool {
    let hash_len = format.raw_len();
    if bytes.len() < 8 + 256 * 4 + 2 * hash_len || bytes.get(..4) != Some(&[0xff, b't', b'O', b'c'])
    {
        return false;
    }
    let count = {
        let start = 8 + 255 * 4;
        u32::from_be_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]) as usize
    };
    let Some(after_oids) = (8usize + 256 * 4).checked_add(count.saturating_mul(hash_len)) else {
        return false;
    };
    let Some(after_crc) = after_oids.checked_add(count.saturating_mul(4)) else {
        return false;
    };
    let Some(after_small_offsets) = after_crc.checked_add(count.saturating_mul(4)) else {
        return false;
    };
    let trailer_start = bytes.len().saturating_sub(2 * hash_len);
    after_small_offsets < trailer_start
}

#[cfg(test)]
mod tests {
    use super::*;
    use sley_object::{EncodedObject, ObjectType};
    use sley_pack::PackFile;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture_pack(object_dir: &Path, format: ObjectFormat, byte: u8, name: &str) -> ObjectId {
        let object = EncodedObject::new(ObjectType::Blob, vec![byte; 32]);
        let oid = object.object_id(format).expect("object id");
        let pack = PackFile::write_undeltified(&[object], format).expect("pack");
        let pack_dir = object_dir.join("pack");
        fs::create_dir_all(&pack_dir).expect("pack directory");
        fs::write(pack_dir.join(format!("{name}.pack")), pack.pack).expect("pack bytes");
        fs::write(pack_dir.join(format!("{name}.idx")), pack.index).expect("index bytes");
        oid
    }

    fn temp_object_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sley-midx-layer-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("object directory");
        path
    }

    #[test]
    fn layer_deduplicates_objects_and_prefers_named_pack() {
        let object_dir = temp_object_dir();
        let oid = fixture_pack(&object_dir, ObjectFormat::Sha1, b'a', "pack-one");
        fixture_pack(&object_dir, ObjectFormat::Sha1, b'a', "pack-two");
        let mut options = MultiPackIndexLayerOptions::new(
            &object_dir,
            ObjectFormat::Sha1,
            vec!["pack-one.idx".into(), "pack-two.idx".into()],
        );
        options.preferred_pack_name = Some("pack-two.pack".into());

        let outcome = build_multi_pack_index_layer(
            options,
            |_| panic!("bitmap inputs must stay lazy"),
            |_| {},
        )
        .expect("build layer");
        let parsed = MultiPackIndex::parse(&outcome.midx, ObjectFormat::Sha1).expect("parse midx");
        assert_eq!(parsed.objects.len(), 1);
        assert_eq!(parsed.objects[0].oid, oid);
        assert_eq!(parsed.objects[0].pack_int_id, 1);
        assert_eq!(outcome.preferred_pack, Some(1));
        fs::remove_dir_all(object_dir).expect("cleanup");
    }

    #[test]
    fn unknown_preferred_pack_is_an_event_not_terminal_output() {
        let object_dir = temp_object_dir();
        fixture_pack(&object_dir, ObjectFormat::Sha1, b'a', "pack-one");
        let mut options = MultiPackIndexLayerOptions::new(
            &object_dir,
            ObjectFormat::Sha1,
            vec!["pack-one.idx".into()],
        );
        options.preferred_pack_name = Some("missing.pack".into());
        let mut events = Vec::new();
        let outcome = build_multi_pack_index_layer(
            options,
            |_| panic!("bitmap inputs must stay lazy"),
            |event| events.push(event),
        )
        .expect("build layer");
        assert_eq!(outcome.preferred_pack, None);
        assert_eq!(
            events,
            vec![MultiPackIndexEvent::UnknownPreferredPack(
                "missing.pack".into()
            )]
        );
        fs::remove_dir_all(object_dir).expect("cleanup");
    }

    #[test]
    fn unchanged_sha256_layer_does_not_resolve_bitmap_inputs() {
        let object_dir = temp_object_dir();
        fixture_pack(&object_dir, ObjectFormat::Sha256, b'a', "pack-one");
        let mut options = MultiPackIndexLayerOptions::new(
            &object_dir,
            ObjectFormat::Sha256,
            vec!["pack-one.idx".into()],
        );
        let initial = build_multi_pack_index_layer(
            options.clone(),
            |_| panic!("non-bitmap build must stay lazy"),
            |_| {},
        )
        .expect("initial SHA-256 layer");
        fs::write(object_dir.join("pack/multi-pack-index"), &initial.midx).expect("install midx");

        options.skip_if_unchanged = true;
        let unchanged = build_multi_pack_index_layer(
            options,
            |_| panic!("unchanged layer must stay lazy"),
            |_| {},
        )
        .expect("unchanged SHA-256 layer");
        assert!(unchanged.unchanged);
        assert_eq!(unchanged.checksum.format(), ObjectFormat::Sha256);
        fs::remove_dir_all(object_dir).expect("cleanup");
    }
}
